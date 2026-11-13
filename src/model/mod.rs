//! Core data types for `beads_rust`.
//!
//! This module defines the fundamental types used throughout the application:
//! - `Issue` - The core work item
//! - `Status` - Issue lifecycle states
//! - `IssueType` - Categories of issues
//! - `Dependency` - Relationships between issues
//! - `Comment` - Issue comments
//! - `Event` - Audit log entries

pub mod acceptance;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::str::FromStr;

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

/// Serialize Option<i32> as 0 when None (for bd conformance - bd expects integer, not null)
#[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
fn serialize_compaction_level<S>(value: &Option<i32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_i32(value.unwrap_or(0))
}

/// Deserialize an optional metadata string, coercing a degenerate empty (or
/// whitespace-only) string to `None`.
///
/// Legacy JSONL written by older `br`/`bd` versions serialized absent
/// dependency metadata as `"metadata":""` rather than omitting the field or
/// writing `"{}"`. The empty string is not valid JSON, so downstream consumers
/// that parse `metadata` as JSON (e.g. the JSONL → SQLite rebuild/import path)
/// would reject such records with an opaque `EOF while parsing` error.
///
/// Treating `""` as absent is lossless: a present-but-empty metadata field
/// carries no information, and the import path already materializes `None` as
/// the empty JSON object `"{}"`. Genuine metadata (any non-blank string) is
/// preserved verbatim so real data is never discarded.
fn deserialize_optional_metadata<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(match value {
        Some(s) if s.trim().is_empty() => None,
        other => other,
    })
}

/// Issue lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Blocked,
    Deferred,
    Draft,
    Closed,
    #[serde(rename = "tombstone")]
    Tombstone,
    #[serde(rename = "pinned")]
    Pinned,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match Self::known_value(&value) {
            Some(status) => status,
            None => Self::Custom(value.to_lowercase()),
        })
    }
}

impl Status {
    fn known_value(value: &str) -> Option<Self> {
        Some(match value.to_lowercase().as_str() {
            "open" => Self::Open,
            "in_progress" | "inprogress" => Self::InProgress,
            "blocked" => Self::Blocked,
            "deferred" => Self::Deferred,
            "draft" => Self::Draft,
            "closed" => Self::Closed,
            "tombstone" => Self::Tombstone,
            "pinned" => Self::Pinned,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Draft => "draft",
            Self::Closed => "closed",
            Self::Tombstone => "tombstone",
            Self::Pinned => "pinned",
            Self::Custom(value) => value,
        }
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Tombstone)
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Open | Self::InProgress)
    }

    /// Returns true if the issue is in draft state (not yet ready for execution).
    #[must_use]
    pub const fn is_draft(&self) -> bool {
        matches!(self, Self::Draft)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for Status {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::known_value(s).unwrap_or_else(|| Self::Custom(s.to_lowercase())))
    }
}

/// Issue priority (0=Critical, 4=Backlog).
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(transparent)]
pub struct Priority(pub i32);

impl Default for Priority {
    fn default() -> Self {
        Self::MEDIUM
    }
}

impl Priority {
    pub const CRITICAL: Self = Self(0);
    pub const HIGH: Self = Self(1);
    pub const MEDIUM: Self = Self(2);
    pub const LOW: Self = Self(3);
    pub const BACKLOG: Self = Self(4);
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P{}", self.0)
    }
}

impl FromStr for Priority {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_uppercase();
        let val = s.strip_prefix('P').unwrap_or(&s);

        match val.parse::<i32>() {
            Ok(p) if (0..=4).contains(&p) => Ok(Self(p)),
            Ok(p) => Err(crate::error::BeadsError::InvalidPriority {
                priority: p.to_string(),
            }),
            Err(_) => Err(crate::error::BeadsError::InvalidPriority {
                priority: val.to_string(),
            }),
        }
    }
}

/// Issue type category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default, JsonSchema)]
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
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for IssueType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match Self::known_value(&value) {
            Some(issue_type) => issue_type,
            None => Self::Custom(value.to_lowercase()),
        })
    }
}

impl IssueType {
    fn known_value(value: &str) -> Option<Self> {
        Some(match value.to_lowercase().as_str() {
            "task" => Self::Task,
            "bug" => Self::Bug,
            "feature" => Self::Feature,
            "epic" => Self::Epic,
            "chore" => Self::Chore,
            "docs" => Self::Docs,
            "question" => Self::Question,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Task => "task",
            Self::Bug => "bug",
            Self::Feature => "feature",
            Self::Epic => "epic",
            Self::Chore => "chore",
            Self::Docs => "docs",
            Self::Question => "question",
            Self::Custom(value) => value,
        }
    }

    /// Returns true if this is a standard (non-custom) issue type.
    /// Used for bd conformance validation in CLI commands.
    #[must_use]
    pub const fn is_standard(&self) -> bool {
        !matches!(self, Self::Custom(_))
    }
}

impl fmt::Display for IssueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for IssueType {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::known_value(s).unwrap_or_else(|| Self::Custom(s.to_lowercase())))
    }
}

/// Dependency relationship type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyType {
    Blocks,
    ParentChild,
    ConditionalBlocks,
    WaitsFor,
    Related,
    DiscoveredFrom,
    RepliesTo,
    RelatesTo,
    Duplicates,
    Supersedes,
    CausedBy,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for DependencyType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let normalized = value.to_lowercase();
        Ok(match normalized.as_str() {
            "blocks" => Self::Blocks,
            "parent-child" => Self::ParentChild,
            "conditional-blocks" => Self::ConditionalBlocks,
            "waits-for" => Self::WaitsFor,
            "related" => Self::Related,
            "discovered-from" => Self::DiscoveredFrom,
            "replies-to" => Self::RepliesTo,
            "relates-to" => Self::RelatesTo,
            "duplicates" => Self::Duplicates,
            "supersedes" => Self::Supersedes,
            "caused-by" => Self::CausedBy,
            _ => Self::Custom(normalized),
        })
    }
}

impl DependencyType {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Blocks => "blocks",
            Self::ParentChild => "parent-child",
            Self::ConditionalBlocks => "conditional-blocks",
            Self::WaitsFor => "waits-for",
            Self::Related => "related",
            Self::DiscoveredFrom => "discovered-from",
            Self::RepliesTo => "replies-to",
            Self::RelatesTo => "relates-to",
            Self::Duplicates => "duplicates",
            Self::Supersedes => "supersedes",
            Self::CausedBy => "caused-by",
            Self::Custom(value) => value,
        }
    }

    #[must_use]
    pub const fn affects_ready_work(&self) -> bool {
        matches!(
            self,
            Self::Blocks | Self::ParentChild | Self::ConditionalBlocks | Self::WaitsFor
        )
    }

    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(
            self,
            Self::Blocks | Self::ParentChild | Self::ConditionalBlocks | Self::WaitsFor
        )
    }
}

impl fmt::Display for DependencyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for DependencyType {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "blocks" => Ok(Self::Blocks),
            "parent-child" => Ok(Self::ParentChild),
            "conditional-blocks" => Ok(Self::ConditionalBlocks),
            "waits-for" => Ok(Self::WaitsFor),
            "related" => Ok(Self::Related),
            "discovered-from" => Ok(Self::DiscoveredFrom),
            "replies-to" => Ok(Self::RepliesTo),
            "relates-to" => Ok(Self::RelatesTo),
            "duplicates" => Ok(Self::Duplicates),
            "supersedes" => Ok(Self::Supersedes),
            "caused-by" => Ok(Self::CausedBy),
            other => Ok(Self::Custom(other.to_string())),
        }
    }
}

/// Audit event type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventType {
    Created,
    Updated,
    StatusChanged,
    PriorityChanged,
    AssigneeChanged,
    Commented,
    CommentEdited,
    CommentRedacted,
    CommentDeleted,
    Closed,
    Reopened,
    DependencyAdded,
    DependencyRemoved,
    LabelAdded,
    LabelRemoved,
    Compacted,
    Deleted,
    Restored,
    Custom(String),
}

impl EventType {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::StatusChanged => "status_changed",
            Self::PriorityChanged => "priority_changed",
            Self::AssigneeChanged => "assignee_changed",
            Self::Commented => "commented",
            Self::CommentEdited => "comment_edited",
            Self::CommentRedacted => "comment_redacted",
            Self::CommentDeleted => "comment_deleted",
            Self::Closed => "closed",
            Self::Reopened => "reopened",
            Self::DependencyAdded => "dependency_added",
            Self::DependencyRemoved => "dependency_removed",
            Self::LabelAdded => "label_added",
            Self::LabelRemoved => "label_removed",
            Self::Compacted => "compacted",
            Self::Deleted => "deleted",
            Self::Restored => "restored",
            Self::Custom(value) => value,
        }
    }
}

impl Serialize for EventType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let normalized = value.to_lowercase();
        let event_type = match normalized.as_str() {
            "created" => Self::Created,
            "updated" => Self::Updated,
            "status_changed" => Self::StatusChanged,
            "priority_changed" => Self::PriorityChanged,
            "assignee_changed" => Self::AssigneeChanged,
            "commented" => Self::Commented,
            "comment_edited" => Self::CommentEdited,
            "comment_redacted" => Self::CommentRedacted,
            "comment_deleted" => Self::CommentDeleted,
            "closed" => Self::Closed,
            "reopened" => Self::Reopened,
            "dependency_added" => Self::DependencyAdded,
            "dependency_removed" => Self::DependencyRemoved,
            "label_added" => Self::LabelAdded,
            "label_removed" => Self::LabelRemoved,
            "compacted" => Self::Compacted,
            "deleted" => Self::Deleted,
            "restored" => Self::Restored,
            _ => Self::Custom(normalized),
        };
        Ok(event_type)
    }
}

impl JsonSchema for EventType {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("EventType")
    }

    fn json_schema(generator: &mut schemars::generate::SchemaGenerator) -> schemars::Schema {
        // EventType serializes as a string (see custom Serialize/Deserialize above).
        generator.subschema_for::<String>()
    }
}

/// The primary issue entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Issue {
    /// Unique ID (e.g., "bd-abc123").
    pub id: String,

    /// Content hash for deduplication and sync.
    #[serde(skip)]
    pub content_hash: Option<String>,

    /// Title (1-500 chars).
    pub title: String,

    /// Detailed description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Technical design notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,

    /// Acceptance criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,

    /// Per-issue prerequisite checklist, separate from acceptance criteria
    /// and dependency edges. Policy decides which transitions require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<String>,

    /// Additional notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,

    /// Workflow status.
    #[serde(default)]
    pub status: Status,

    /// Priority (0=Critical, 4=Backlog).
    #[serde(default)]
    pub priority: Priority,

    /// Issue type (bug, feature, etc.).
    #[serde(default)]
    pub issue_type: IssueType,

    /// Assigned user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,

    /// Issue owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    /// Estimated effort in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<i32>,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Creator username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,

    /// Closure timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,

    /// Reason for closure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,

    /// Session ID that closed this issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by_session: Option<String>,

    /// Actor that performed the current close (GitHub #584). Missing means
    /// unknown, including historical closes predating durable attribution.
    /// Backed by `close_metadata` and shared in JSONL, never inferred from the
    /// creator, assignee, importing actor, or unrelated local event history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<String>,

    /// Self-reported agent name captured when the issue was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by_agent_name: Option<String>,

    /// Self-reported harness captured when the issue was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by_harness: Option<String>,

    /// Self-reported model captured when the issue was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by_model: Option<String>,

    /// Close-policy bypass audit trail (GitHub #474). Serialized into the
    /// JSONL export only when the close bypassed policy so a bypassed close
    /// is reviewable off-machine; absent means "not bypassed" (pre-change
    /// exports stay valid). Backed by the DB-only `close_metadata` table;
    /// excluded from `content_hash` and from `sync_equals`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypassed_policy: Option<bool>,

    /// Operator-supplied reason for the policy bypass (GitHub #474).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_reason: Option<String>,

    /// Names of the close-policy gates the bypass overrode (GitHub #474).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_gates_fired: Option<Vec<String>>,

    /// Due date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,

    /// Defer until date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<DateTime<Utc>>,

    /// External reference (e.g., JIRA-123).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,

    /// Source system for imported issues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_system: Option<String>,

    /// Source repository for multi-repo support — basename of the
    /// canonicalized parent of `.beads/`. Stable across clones of the
    /// same repo on different machines (different absolute paths
    /// produce the same basename). See [`canonical_source_repo`] in
    /// `cli::commands::create`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,

    /// Absolute canonical path of the source repository. Distinct from
    /// `source_repo`: this field uniquely identifies the workspace on
    /// the machine that produced the issue, which is what multi-repo
    /// fleet automation needs to route beads back to the right
    /// directory (see beads_rust#289). Two clones of the same repo
    /// under `~/Developer/foo` vs `~/Developer/scratch/foo` collide on
    /// `source_repo` but disagree here. Optional — older databases and
    /// hand-edited JSONL records without this field are valid.
    ///
    /// Machine-local (beads_rust#528): it lives in the database and in
    /// command JSON output, but the JSONL export never carries it, so the
    /// committed file does not leak local paths or churn between machines.
    /// A legacy JSONL row that still carries a value imports it into a new
    /// row or a row without a path, but never replaces this machine's own
    /// value. Because it is not part of the synced payload, `sync_equals`
    /// ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo_path: Option<String>,

    /// Canonical-JSON governing instructions inherited by descendant
    /// beads (beads_rust#297). When set on an ancestor and the project
    /// has `inherited_context.enabled = true` in `.beads/config.yaml`,
    /// `br update --status in_progress` / `--claim` and `br show` emit
    /// the ancestor's `agent_context` alongside the child's normal
    /// output so the working agent sees the constraints regardless of
    /// context compaction or cold-start lookups.
    ///
    /// Storage: TEXT column holding a JSON document (typically an
    /// object with `skills`, `constraints`, `references`, `workflow`,
    /// `metadata` fields, but the schema is intentionally open).
    /// `None` means "no inherited context"; emission for descendants
    /// silently skips ancestors with `None` (no error, no noise).
    ///
    /// Not displayed in `br list` / `br search` — this is per-bead
    /// governance metadata, not browsable content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_context: Option<String>,

    // Tombstone fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_type: Option<String>,

    // Compaction (legacy/compat)
    // Note: Always serialize compaction_level as integer (0 when None) for bd conformance
    // bd's Go sql scanner cannot handle NULL for integer columns
    #[serde(default, serialize_with = "serialize_compaction_level")]
    pub compaction_level: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_size: Option<i32>,

    // Messaging
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,

    // Context
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_template: bool,

    // Relations (for export/display, not always in DB table directly)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dependencies: Vec<Dependency>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comments: Vec<Comment>,
}

impl Default for Issue {
    fn default() -> Self {
        Self {
            id: String::new(),
            content_hash: None,
            title: String::new(),
            description: None,
            design: None,
            acceptance_criteria: None,
            prerequisites: None,
            notes: None,
            status: Status::default(),
            priority: Priority::default(),
            issue_type: IssueType::default(),
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            closed_by: None,
            closed_by_agent_name: None,
            closed_by_harness: None,
            closed_by_model: None,
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo_path: None,
            agent_context: None,
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
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }
}

impl Issue {
    /// Compute the deterministic content hash for this issue.
    ///
    /// Uses br's canonical field order with length-prefixed field encoding for
    /// unambiguous deduplication.
    /// Excludes IDs, timestamps, relations, and tombstone metadata.
    ///
    /// Delegates to [`crate::util::hash::content_hash`] for the canonical implementation.
    #[must_use]
    pub fn compute_content_hash(&self) -> String {
        crate::util::content_hash(self)
    }

    /// Compare two issues using sync semantics instead of raw struct equality.
    ///
    /// This ignores derived or volatile audit fields that would otherwise make
    /// semantically identical issues look different across import/export
    /// boundaries, while still comparing the full synced payload including
    /// labels, dependencies, comments, and user-visible timestamps like `due_at`.
    #[must_use]
    pub fn sync_equals(&self, other: &Self) -> bool {
        if self.id != other.id
            || self.title != other.title
            || self.description != other.description
            || self.design != other.design
            || self.acceptance_criteria != other.acceptance_criteria
            || self.prerequisites != other.prerequisites
            || self.notes != other.notes
            || self.status != other.status
            || self.priority != other.priority
            || self.issue_type != other.issue_type
            || self.assignee != other.assignee
            || self.owner != other.owner
            || self.estimated_minutes != other.estimated_minutes
            || self.created_by != other.created_by
            || self.closed_at != other.closed_at
            || self.close_reason != other.close_reason
            || self.closed_by_session != other.closed_by_session
            || self.closed_by != other.closed_by
            || self.closed_by_agent_name != other.closed_by_agent_name
            || self.closed_by_harness != other.closed_by_harness
            || self.closed_by_model != other.closed_by_model
            || self.due_at != other.due_at
            || self.defer_until != other.defer_until
            || self.external_ref != other.external_ref
            || self.source_system != other.source_system
            || self.source_repo != other.source_repo
            || self.deleted_at != other.deleted_at
            || self.deleted_by != other.deleted_by
            || self.delete_reason != other.delete_reason
            || self.original_type != other.original_type
            || self.compacted_at != other.compacted_at
            || self.compacted_at_commit != other.compacted_at_commit
            || self.original_size != other.original_size
            || self.sender != other.sender
            || self.ephemeral != other.ephemeral
            || self.pinned != other.pinned
            || self.is_template != other.is_template
            // Inherited governance context (beads_rust#297): a live,
            // JSONL-serialized payload field, so an agent_context-only change
            // must count as a difference. Omitting it here silently dropped a
            // peer's context update during three-way merge / import de-dup
            // (persisted_import_issue_equals had to re-add it as a workaround).
            || self.agent_context != other.agent_context
        {
            return false;
        }

        // Handle compaction_level serialization quirk where None == 0
        if self.compaction_level.unwrap_or(0) != other.compaction_level.unwrap_or(0) {
            return false;
        }

        // Fast path for relations: if lengths differ, they can't be equal
        if self.dependencies.len() != other.dependencies.len()
            || self.comments.len() != other.comments.len()
        {
            return false;
        }

        // Compare labels (order independent)
        let mut self_labels = self.labels.clone();
        self_labels.sort_unstable();
        self_labels.dedup();
        let mut other_labels = other.labels.clone();
        other_labels.sort_unstable();
        other_labels.dedup();
        if self_labels != other_labels {
            return false;
        }

        // Compare dependencies (order independent)
        let mut self_deps = self.dependencies.clone();
        self_deps.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        let mut other_deps = other.dependencies.clone();
        other_deps.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        if self_deps != other_deps {
            return false;
        }

        comments_sync_equal(&self.comments, &other.comments)
    }

    /// Check if this issue is a tombstone that has exceeded its TTL.
    #[must_use]
    pub fn is_expired_tombstone(&self, retention_days: Option<u64>) -> bool {
        self.is_expired_tombstone_at(retention_days, Utc::now())
    }

    /// Check if this issue is a tombstone that has exceeded its TTL at a
    /// caller-supplied cutoff.
    ///
    /// Supplying the cutoff lets one logical export apply a single retention
    /// decision to every issue, even when serial and parallel preparation take
    /// different amounts of time.
    #[must_use]
    pub fn is_expired_tombstone_at(
        &self,
        retention_days: Option<u64>,
        as_of: DateTime<Utc>,
    ) -> bool {
        if self.status != Status::Tombstone {
            return false;
        }

        // Automatic retention must not discard the evidence that suppresses
        // an older peer's sensitive comment payload.
        if self
            .comments
            .iter()
            .any(|comment| comment.state() != CommentState::Active)
        {
            return false;
        }

        tombstone_expired_at(self.deleted_at, retention_days, as_of)
    }
}

/// Apply the tombstone TTL to a persisted deletion timestamp without loading
/// the rest of an issue. The caller must first establish tombstone status.
pub(crate) fn tombstone_expired_at(
    deleted_at: Option<DateTime<Utc>>,
    retention_days: Option<u64>,
    as_of: DateTime<Utc>,
) -> bool {
    let Some(days) = retention_days else {
        return false;
    };

    if days == 0 {
        return false; // Keep forever when retention is disabled.
    }

    let Some(deleted_at) = deleted_at else {
        return false; // Keep if deletion time is unknown.
    };

    // Clamp days to a safe maximum to avoid panic in Duration::days().
    // chrono::Duration can handle up to ~292,000 years, but we'll clamp to
    // something extremely safe for an issue tracker (e.g., 1000 years).
    let max_safe_days = 365_u64 * 1000;
    let days_i64 = i64::try_from(days.min(max_safe_days)).unwrap_or(365_000);
    let Some(expiration_time) = deleted_at.checked_add_signed(chrono::Duration::days(days_i64))
    else {
        // A positive retention period that extends beyond chrono's maximum
        // representable timestamp has not expired at any representable
        // cutoff, so retain the tombstone.
        return false;
    };
    as_of > expiration_time
}

/// Epic completion status with child counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct EpicStatus {
    pub epic: Issue,
    pub total_children: usize,
    pub closed_children: usize,
    pub eligible_for_close: bool,
}

/// Relationship between two issues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Dependency {
    /// The issue that has the dependency (source).
    pub issue_id: String,

    /// The issue being depended on (target).
    pub depends_on_id: String,

    /// Type of dependency.
    #[serde(rename = "type")]
    pub dep_type: DependencyType,

    /// Creation timestamp.
    pub created_at: DateTime<Utc>,

    /// Creator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// Optional metadata (JSON).
    ///
    /// A degenerate empty (or whitespace-only) string is coerced to `None` on
    /// deserialization to tolerate legacy JSONL that wrote `"metadata":""`
    /// instead of omitting the field. See [`deserialize_optional_metadata`].
    #[serde(
        default,
        deserialize_with = "deserialize_optional_metadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<String>,

    /// Thread ID for conversation linking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A comment on an issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Comment {
    /// Database-local rowid. It is exported to JSONL for reference only:
    /// every clone mints its own sequence, so two clones' comments routinely
    /// share an id and import reassigns the local rowid when that happens.
    /// Sync compares portable revision identities, never this rowid.
    pub id: i64,
    pub issue_id: String,
    pub author: String,
    #[serde(rename = "text")]
    pub body: String,
    pub created_at: DateTime<Utc>,
    /// Portable identity and content-free revision ancestry. Absent in legacy
    /// records; enrolled deterministically before mutation or reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<CommentRevision>,
}

/// Removal is monotone: deletion dominates redaction, and either suppresses
/// every ordinary edit independently of clocks.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CommentState {
    #[default]
    Active,
    Redacted,
    Deleted,
}

/// Current revision metadata. Ancestors are opaque digests, never old text.
/// They distinguish sequential edits from divergent edits even when one branch
/// has made more edits or has a clock ahead of another machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentRevision {
    pub identity: String,
    pub generation: u64,
    pub head: String,
    pub ancestors: Vec<String>,
    pub state: CommentState,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

fn comment_conflict(message: impl Into<String>) -> crate::error::BeadsError {
    crate::error::BeadsError::SyncConflict {
        message: message.into(),
    }
}

fn comment_digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(part);
    }
    crate::util::hex_encode(&digest.finalize())
}

fn is_comment_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Comment {
    /// Whether this comment is visible text, redacted, or logically deleted.
    #[must_use]
    pub fn state(&self) -> CommentState {
        self.revision
            .as_ref()
            .map_or(CommentState::Active, |r| r.state)
    }

    #[must_use]
    pub fn revision_head(&self) -> Option<&str> {
        self.revision.as_ref().map(|r| r.head.as_str())
    }

    /// Text suitable for display, including a text-free terminal marker.
    #[must_use]
    pub fn display_body(&self) -> &str {
        match self.state() {
            CommentState::Active => &self.body,
            CommentState::Redacted => "[redacted]",
            CommentState::Deleted => "[deleted]",
        }
    }

    /// Validate portable metadata without echoing comment contents.
    ///
    /// # Errors
    /// Returns a conflict for malformed ancestry or a nonempty removed body.
    pub fn validate_revision(&self) -> crate::error::Result<()> {
        let Some(revision) = &self.revision else {
            return Ok(());
        };
        if !is_comment_digest(&revision.identity)
            || !is_comment_digest(&revision.head)
            || revision.ancestors.len() > 4096
            || revision
                .ancestors
                .iter()
                .any(|ancestor| !is_comment_digest(ancestor) || ancestor == &revision.head)
            || !revision.ancestors.windows(2).all(|pair| pair[0] < pair[1])
            || revision.updated_by.trim().is_empty()
            || revision.updated_by.len() > 200
            || revision.updated_by.contains('\0')
        {
            return Err(comment_conflict("Invalid comment revision metadata"));
        }
        if revision.generation == 0 {
            if revision.head != revision.identity
                || !revision.ancestors.is_empty()
                || revision.state != CommentState::Active
                || revision.updated_at != self.created_at
                || revision.updated_by != self.author
            {
                return Err(comment_conflict("Invalid initial comment revision"));
            }
        } else if !revision.ancestors.contains(&revision.identity) {
            return Err(comment_conflict("Comment revision is missing its origin"));
        }
        if revision.state != CommentState::Active && !self.body.is_empty() {
            return Err(comment_conflict("Removed comment must have an empty body"));
        }
        Ok(())
    }

    /// Replace text or advance to a terminal privacy state. Enroll the whole
    /// issue's legacy comments first, then persist this revision and its audit
    /// in one transaction.
    ///
    /// # Errors
    /// Refuses invalid metadata, empty active text, terminal-state downgrades,
    /// or exhausted ordinary-edit ancestry. The comment stays unchanged.
    pub fn revise(
        &mut self,
        state: CommentState,
        body: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> crate::error::Result<()> {
        self.validate_revision()?;
        let previous = self
            .revision
            .as_ref()
            .ok_or_else(|| comment_conflict("Comment must be enrolled before modification"))?;
        if previous.state != CommentState::Active && state < previous.state {
            return Err(comment_conflict(
                "A redacted or deleted comment cannot be restored; add a new comment",
            ));
        }
        if (state == CommentState::Active && body.trim().is_empty())
            || (state != CommentState::Active && !body.is_empty())
            || body.contains('\0')
            || actor.trim().is_empty()
            || actor.len() > 200
            || actor.contains('\0')
        {
            return Err(crate::error::BeadsError::validation(
                "comment",
                "invalid comment text or actor",
            ));
        }
        if previous.state == state && self.body == body {
            return Ok(());
        }
        let mut revision = previous.clone();
        if state == CommentState::Active {
            revision.generation = revision
                .generation
                .checked_add(1)
                .ok_or_else(|| comment_conflict("Comment revision generation exhausted"))?;
            if revision.ancestors.len() >= 4096 {
                return Err(comment_conflict("Comment revision ancestry limit reached"));
            }
            revision.ancestors.push(revision.head.clone());
            revision.ancestors.sort();
            revision.ancestors.dedup();
        } else {
            // Privacy removal remains possible at the ordinary edit limit.
            // A terminal state needs only its origin to suppress every branch.
            revision.generation = revision.generation.saturating_add(1);
            revision.ancestors = vec![revision.identity.clone()];
        }
        revision.state = state;
        revision.updated_at = now;
        actor.clone_into(&mut revision.updated_by);
        let state_name = match state {
            CommentState::Active => "active",
            CommentState::Redacted => "redacted",
            CommentState::Deleted => "deleted",
        };
        revision.head = comment_digest(&[
            b"br-comment-revision-v1",
            revision.identity.as_bytes(),
            &revision.generation.to_le_bytes(),
            previous.head.as_bytes(),
            state_name.as_bytes(),
            now.to_rfc3339().as_bytes(),
            actor.as_bytes(),
            body.as_bytes(),
        ]);
        body.clone_into(&mut self.body);
        self.revision = Some(revision);
        Ok(())
    }
}

/// Enroll legacy comments without changing IDs, authors, creation times or
/// text. Original-payload digests match old JSONL after an edit. Occurrence
/// ordinals preserve intentional identical comments; persist all siblings
/// when modifying one so their ordinals cannot shift.
///
/// # Errors
/// Rejects malformed existing revision metadata.
pub fn normalize_comment_revisions(comments: &mut [Comment]) -> crate::error::Result<()> {
    let enrolled: HashSet<String> = comments
        .iter()
        .filter_map(|comment| {
            comment
                .revision
                .as_ref()
                .map(|revision| revision.identity.clone())
        })
        .collect();
    let mut order: Vec<usize> = (0..comments.len()).collect();
    order.sort_by_key(|index| (comments[*index].id, *index));
    let mut occurrences = BTreeMap::<String, u64>::new();
    for index in order {
        let comment = &mut comments[index];
        if comment.revision.is_none() {
            let original = comment_digest(&[
                b"br-comment-origin-v1",
                comment.issue_id.as_bytes(),
                comment.created_at.to_rfc3339().as_bytes(),
                comment.author.as_bytes(),
                comment.body.as_bytes(),
            ]);
            let occurrence = occurrences.entry(original.clone()).or_default();
            let identity = comment_digest(&[original.as_bytes(), &occurrence.to_le_bytes()]);
            *occurrence += 1;
            if enrolled.contains(&identity) {
                // Advancing past an enrolled terminal identity would turn a
                // stale body into a new live comment. Collapsing the entries
                // could instead lose an intentional duplicate. Require an
                // unambiguous complete snapshot before importing this mixture.
                return Err(comment_conflict(
                    "Ambiguous mixture of legacy and versioned comments; reconcile complete snapshots separately",
                ));
            }
            comment.revision = Some(CommentRevision {
                head: identity.clone(),
                identity,
                generation: 0,
                ancestors: Vec::new(),
                state: CommentState::Active,
                updated_at: comment.created_at,
                updated_by: comment.author.clone(),
            });
        }
        comment.validate_revision()?;
    }
    Ok(())
}

/// Compare portable state, ignoring database-local rowids and input order.
#[must_use]
pub fn comments_sync_equal(left: &[Comment], right: &[Comment]) -> bool {
    let normalized = |source: &[Comment]| -> crate::error::Result<Vec<Comment>> {
        let mut comments = source.to_vec();
        normalize_comment_revisions(&mut comments)?;
        for comment in &mut comments {
            comment.id = 0;
        }
        comments.sort_by(|a, b| a.revision.cmp(&b.revision));
        Ok(comments)
    };
    match (normalized(left), normalized(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn join_comment_revision(left: &Comment, right: &Comment) -> crate::error::Result<Comment> {
    let a = left.revision.as_ref().expect("normalized comment");
    let b = right.revision.as_ref().expect("normalized comment");
    if left.author != right.author || left.created_at != right.created_at {
        return Err(comment_conflict(format!(
            "Comment {} has conflicting immutable attribution",
            a.identity
        )));
    }
    let winner = if a.head == b.head {
        if left.body != right.body
            || a.state != b.state
            || a.generation != b.generation
            || a.updated_at != b.updated_at
            || a.updated_by != b.updated_by
        {
            return Err(comment_conflict(format!(
                "Comment {} has inconsistent revision data",
                a.identity
            )));
        }
        left
    } else if a.state != CommentState::Active || b.state != CommentState::Active {
        // Privacy dominates longer and future-clock active edits. Same-state
        // concurrent removals have no text to lose.
        if (a.state, a.generation, a.updated_at, &a.head)
            >= (b.state, b.generation, b.updated_at, &b.head)
        {
            left
        } else {
            right
        }
    } else {
        match (a.ancestors.contains(&b.head), b.ancestors.contains(&a.head)) {
            (true, false) if a.generation > b.generation => left,
            (false, true) if b.generation > a.generation => right,
            _ => {
                return Err(comment_conflict(format!(
                    "Concurrent edits to comment {}; reconcile its revisions explicitly before syncing",
                    a.identity
                )));
            }
        }
    };
    let mut merged = winner.clone();
    let revision = merged.revision.as_mut().expect("normalized comment");
    if revision.state == CommentState::Active {
        let mut ancestors: BTreeSet<String> =
            a.ancestors.iter().chain(&b.ancestors).cloned().collect();
        ancestors.insert(a.head.clone());
        ancestors.insert(b.head.clone());
        ancestors.remove(&revision.head);
        revision.ancestors = ancestors.into_iter().collect();
    } else {
        revision.ancestors = vec![revision.identity.clone()];
    }
    merged.validate_revision()?;
    Ok(merged)
}

/// Reconcile revisions independently of parent issue timestamps and winner
/// flags. Ordinary concurrent edits fail closed; terminal privacy states win.
/// Existing local IDs survive matched revisions, with unrelated collisions
/// left for the storage allocator.
///
/// # Errors
/// Returns content-free conflicts for divergent active revisions or invalid
/// identity/attribution. Never copies losing text into an audit or error.
///
/// # Panics
/// Panics if internal normalization leaves a comment without revision metadata.
pub fn merge_comment_lists(
    primary: &[Comment],
    secondary: &[Comment],
    target_id: &str,
) -> crate::error::Result<Vec<Comment>> {
    let mut primary = primary.to_vec();
    let mut secondary = secondary.to_vec();
    normalize_comment_revisions(&mut primary)?;
    normalize_comment_revisions(&mut secondary)?;
    let primary_ids: BTreeMap<String, i64> = primary
        .iter()
        .filter(|comment| comment.id > 0)
        .map(|comment| {
            (
                comment
                    .revision
                    .as_ref()
                    .expect("normalized comment")
                    .identity
                    .clone(),
                comment.id,
            )
        })
        .collect();
    let reserved_ids: HashSet<i64> = primary_ids.values().copied().collect();
    let mut groups: BTreeMap<String, Vec<Comment>> = BTreeMap::new();
    for mut comment in primary.into_iter().chain(secondary) {
        let identity = comment
            .revision
            .as_ref()
            .expect("normalized comment")
            .identity
            .clone();
        target_id.clone_into(&mut comment.issue_id);
        groups.entry(identity).or_default().push(comment);
    }
    let mut merged = Vec::with_capacity(groups.len());
    for (identity, mut candidates) in groups {
        // A peer's older creation time must not let it steal a local numeric
        // target. Reserve the primary database's IDs before sorting output.
        let local_id = primary_ids.get(&identity).copied().unwrap_or_else(|| {
            candidates
                .iter()
                .find(|comment| comment.id > 0 && !reserved_ids.contains(&comment.id))
                .map_or(0, |comment| comment.id)
        });
        // Apply terminal evidence first, even if one input has divergent live
        // copies of the same identity.
        candidates.sort_by_key(|comment| std::cmp::Reverse(comment.state()));
        let mut current = candidates[0].clone();
        for candidate in &candidates[1..] {
            current = join_comment_revision(&current, candidate)?;
        }
        current.id = local_id;
        merged.push(current);
    }
    merged.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| {
                a.revision
                    .as_ref()
                    .map(|r| &r.identity)
                    .cmp(&b.revision.as_ref().map(|r| &r.identity))
            })
    });
    let mut seen = HashSet::new();
    for comment in &mut merged {
        if comment.id > 0 && !seen.insert(comment.id) {
            comment.id = 0;
        }
    }
    Ok(merged)
}

/// An event in the issue's history (audit log).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Event {
    pub id: i64,
    pub issue_id: String,
    pub event_type: EventType,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Tier 1 attribution: self-reported agent name (issue #312, Layer 3
    /// capture-only). Recorded as an audit trail; never gated/enforced on.
    /// `None` when the mutating command supplied no attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Tier 1 attribution: self-reported harness identifier (capture-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Tier 1 attribution: self-reported model identifier (capture-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn status_custom_roundtrip() {
        let status: Status = serde_json::from_str("\"custom_status\"").unwrap();
        assert_eq!(status, Status::Custom("custom_status".to_string()));
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"custom_status\"");

        let mixed_case: Status = serde_json::from_str("\"QaReview\"").unwrap();
        assert_eq!(mixed_case, Status::Custom("qareview".to_string()));
    }

    #[test]
    fn issue_type_custom_deserialize_normalizes_spelling() {
        let issue_type: IssueType = serde_json::from_str("\"Odd_Type\"").unwrap();
        assert_eq!(issue_type, IssueType::Custom("odd_type".to_string()));
    }

    #[test]
    fn dependency_type_custom_deserialize_normalizes_spelling() {
        let dep_type: DependencyType = serde_json::from_str("\"Odd-Dep\"").unwrap();
        assert_eq!(dep_type, DependencyType::Custom("odd-dep".to_string()));
    }

    #[test]
    fn issue_deserialize_defaults_missing_fields() {
        let json = r#"{
            "id": "bd-123",
            "title": "Test issue",
            "status": "open",
            "priority": 2,
            "issue_type": "task",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.description.is_none());
        assert!(issue.labels.is_empty());
        assert!(issue.dependencies.is_empty());
        assert!(issue.comments.is_empty());
    }

    #[test]
    fn closure_attribution_is_optional_and_part_of_sync_identity() {
        let legacy: Issue = serde_json::from_str(
            r#"{"id":"bd-old","title":"Historical close","status":"closed",
                "created_at":"2026-01-01T00:00:00Z",
                "updated_at":"2026-01-02T00:00:00Z",
                "closed_at":"2026-01-02T00:00:00Z"}"#,
        )
        .unwrap();
        let legacy_json = serde_json::to_value(&legacy).unwrap();
        for field in [
            "closed_by",
            "closed_by_agent_name",
            "closed_by_harness",
            "closed_by_model",
        ] {
            assert!(
                legacy_json.get(field).is_none(),
                "{field} must stay unknown"
            );
            let mut attributed_json = legacy_json.clone();
            attributed_json[field] = serde_json::json!("agent-value");
            let attributed: Issue = serde_json::from_value(attributed_json).unwrap();
            assert!(!legacy.sync_equals(&attributed), "{field} is shared state");
            assert_eq!(
                legacy.compute_content_hash(),
                attributed.compute_content_hash(),
                "attribution must not alter content-based deduplication"
            );
            let round_trip: Issue =
                serde_json::from_str(&serde_json::to_string(&attributed).unwrap()).unwrap();
            assert!(round_trip.sync_equals(&attributed));
        }
    }

    #[test]
    fn dependency_type_affects_ready_work() {
        assert!(DependencyType::Blocks.affects_ready_work());
        assert!(DependencyType::ParentChild.affects_ready_work());
        assert!(!DependencyType::Related.affects_ready_work());
    }

    #[test]
    fn test_issue_serialization() {
        let issue = Issue {
            id: "bd-123".to_string(),
            content_hash: Some("abc".to_string()),
            title: "Test Issue".to_string(),
            description: Some("Desc".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            prerequisites: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: None,
            updated_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            closed_by: None,
            closed_by_agent_name: None,
            closed_by_harness: None,
            closed_by_model: None,
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_repo_path: None,
            agent_context: None,
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

        let json = serde_json::to_string(&issue).unwrap();
        // Check key fields
        assert!(json.contains("\"id\":\"bd-123\""));
        assert!(json.contains("\"title\":\"Test Issue\""));
        assert!(json.contains("\"status\":\"open\""));
        assert!(json.contains("\"priority\":2"));
        assert!(json.contains("\"issue_type\":\"task\""));
        // Check omission
        assert!(!json.contains("content_hash"));
        assert!(!json.contains("design"));
        assert!(!json.contains("labels")); // empty vec skipped
    }

    #[test]
    fn test_priority_serialization() {
        let p = Priority::CRITICAL;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "0");
    }

    #[test]
    fn test_dependency_type_serialization() {
        let d = DependencyType::Blocks;
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"blocks\"");

        let d = DependencyType::ParentChild;
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"parent-child\"");
    }

    #[test]
    fn test_event_type_serialization() {
        let e = EventType::StatusChanged;
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, "\"status_changed\"");

        let e = EventType::Custom("foobar".to_string());
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, "\"foobar\"");
    }

    // ========================================================================
    // STATUS ENUM TESTS
    // ========================================================================

    #[test]
    fn test_status_from_str_open() {
        assert_eq!(Status::from_str("open").unwrap(), Status::Open);
        assert_eq!(Status::from_str("OPEN").unwrap(), Status::Open);
        assert_eq!(Status::from_str("Open").unwrap(), Status::Open);
    }

    #[test]
    fn test_status_from_str_in_progress() {
        assert_eq!(Status::from_str("in_progress").unwrap(), Status::InProgress);
        assert_eq!(Status::from_str("IN_PROGRESS").unwrap(), Status::InProgress);
        assert_eq!(Status::from_str("inprogress").unwrap(), Status::InProgress);
    }

    #[test]
    fn test_status_from_str_blocked() {
        assert_eq!(Status::from_str("blocked").unwrap(), Status::Blocked);
        assert_eq!(Status::from_str("BLOCKED").unwrap(), Status::Blocked);
    }

    #[test]
    fn test_status_from_str_deferred() {
        assert_eq!(Status::from_str("deferred").unwrap(), Status::Deferred);
        assert_eq!(Status::from_str("DEFERRED").unwrap(), Status::Deferred);
    }

    #[test]
    fn test_status_from_str_closed() {
        assert_eq!(Status::from_str("closed").unwrap(), Status::Closed);
        assert_eq!(Status::from_str("CLOSED").unwrap(), Status::Closed);
    }

    #[test]
    fn test_status_from_str_tombstone() {
        assert_eq!(Status::from_str("tombstone").unwrap(), Status::Tombstone);
        assert_eq!(Status::from_str("TOMBSTONE").unwrap(), Status::Tombstone);
    }

    #[test]
    fn test_status_from_str_pinned() {
        assert_eq!(Status::from_str("pinned").unwrap(), Status::Pinned);
        assert_eq!(Status::from_str("PINNED").unwrap(), Status::Pinned);
    }

    #[test]
    fn test_status_from_str_unknown_becomes_custom() {
        let result = Status::from_str("invalid_status").unwrap();
        assert_eq!(result, Status::Custom("invalid_status".to_string()));

        let mixed_case = Status::from_str("QaReview").unwrap();
        assert_eq!(mixed_case, Status::Custom("qareview".to_string()));
    }

    #[test]
    fn test_status_display() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in_progress");
        assert_eq!(Status::Blocked.to_string(), "blocked");
        assert_eq!(Status::Deferred.to_string(), "deferred");
        assert_eq!(Status::Closed.to_string(), "closed");
        assert_eq!(Status::Tombstone.to_string(), "tombstone");
        assert_eq!(Status::Pinned.to_string(), "pinned");
        assert_eq!(Status::Custom("custom".to_string()).to_string(), "custom");
    }

    #[test]
    fn test_status_is_terminal() {
        assert!(Status::Closed.is_terminal());
        assert!(Status::Tombstone.is_terminal());
        assert!(!Status::Open.is_terminal());
        assert!(!Status::InProgress.is_terminal());
        assert!(!Status::Blocked.is_terminal());
        assert!(!Status::Deferred.is_terminal());
        assert!(!Status::Pinned.is_terminal());
        assert!(!Status::Custom("custom".to_string()).is_terminal());
    }

    #[test]
    fn test_status_is_active() {
        assert!(Status::Open.is_active());
        assert!(Status::InProgress.is_active());
        assert!(!Status::Blocked.is_active());
        assert!(!Status::Deferred.is_active());
        assert!(!Status::Closed.is_active());
        assert!(!Status::Tombstone.is_active());
        assert!(!Status::Pinned.is_active());
        assert!(!Status::Custom("custom".to_string()).is_active());
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(Status::Open.as_str(), "open");
        assert_eq!(Status::InProgress.as_str(), "in_progress");
        assert_eq!(Status::Blocked.as_str(), "blocked");
        assert_eq!(Status::Deferred.as_str(), "deferred");
        assert_eq!(Status::Closed.as_str(), "closed");
        assert_eq!(Status::Tombstone.as_str(), "tombstone");
        assert_eq!(Status::Pinned.as_str(), "pinned");
        assert_eq!(
            Status::Custom("my_status".to_string()).as_str(),
            "my_status"
        );
    }

    // ========================================================================
    // PRIORITY TESTS
    // ========================================================================

    #[test]
    fn test_priority_from_str_with_p_prefix() {
        assert_eq!(Priority::from_str("P0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("P1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("P2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("P3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("P4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_lowercase_p_prefix() {
        assert_eq!(Priority::from_str("p0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("p1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("p2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("p3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("p4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_without_prefix() {
        assert_eq!(Priority::from_str("0").unwrap(), Priority::CRITICAL);
        assert_eq!(Priority::from_str("1").unwrap(), Priority::HIGH);
        assert_eq!(Priority::from_str("2").unwrap(), Priority::MEDIUM);
        assert_eq!(Priority::from_str("3").unwrap(), Priority::LOW);
        assert_eq!(Priority::from_str("4").unwrap(), Priority::BACKLOG);
    }

    #[test]
    fn test_priority_from_str_invalid_too_high() {
        let result = Priority::from_str("5");
        assert!(result.is_err());
        let result = Priority::from_str("P5");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_from_str_invalid_negative() {
        let result = Priority::from_str("-1");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_from_str_invalid_text() {
        let result = Priority::from_str("high");
        assert!(result.is_err());
        let result = Priority::from_str("critical");
        assert!(result.is_err());
    }

    #[test]
    fn test_priority_display() {
        assert_eq!(Priority::CRITICAL.to_string(), "P0");
        assert_eq!(Priority::HIGH.to_string(), "P1");
        assert_eq!(Priority::MEDIUM.to_string(), "P2");
        assert_eq!(Priority::LOW.to_string(), "P3");
        assert_eq!(Priority::BACKLOG.to_string(), "P4");
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::CRITICAL < Priority::HIGH);
        assert!(Priority::HIGH < Priority::MEDIUM);
        assert!(Priority::MEDIUM < Priority::LOW);
        assert!(Priority::LOW < Priority::BACKLOG);
    }

    #[test]
    fn test_priority_default() {
        let p = Priority::default();
        assert_eq!(p, Priority(2));
    }

    // ========================================================================
    // ISSUE TYPE TESTS
    // ========================================================================

    #[test]
    fn test_issue_type_from_str_all_variants() {
        // Only these 5 types are valid via FromStr for bd conformance
        assert_eq!(IssueType::from_str("task").unwrap(), IssueType::Task);
        assert_eq!(IssueType::from_str("bug").unwrap(), IssueType::Bug);
        assert_eq!(IssueType::from_str("feature").unwrap(), IssueType::Feature);
        assert_eq!(IssueType::from_str("epic").unwrap(), IssueType::Epic);
        assert_eq!(IssueType::from_str("chore").unwrap(), IssueType::Chore);
        // docs and question are now accepted
        assert_eq!(IssueType::from_str("docs").unwrap(), IssueType::Docs);
        assert_eq!(
            IssueType::from_str("question").unwrap(),
            IssueType::Question
        );
    }

    #[test]
    fn test_issue_type_from_str_case_insensitive() {
        assert_eq!(IssueType::from_str("TASK").unwrap(), IssueType::Task);
        assert_eq!(IssueType::from_str("BUG").unwrap(), IssueType::Bug);
        assert_eq!(IssueType::from_str("Feature").unwrap(), IssueType::Feature);
    }

    #[test]
    fn test_issue_type_from_str_custom_accepted() {
        // Custom/unknown types are accepted as IssueType::Custom
        let result = IssueType::from_str("custom_type");
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            IssueType::Custom("custom_type".to_string())
        );

        let mixed_case = IssueType::from_str("Odd_Type").unwrap();
        assert_eq!(mixed_case, IssueType::Custom("odd_type".to_string()));
    }

    #[test]
    fn test_issue_type_display() {
        assert_eq!(IssueType::Task.to_string(), "task");
        assert_eq!(IssueType::Bug.to_string(), "bug");
        assert_eq!(IssueType::Feature.to_string(), "feature");
        assert_eq!(IssueType::Epic.to_string(), "epic");
        assert_eq!(IssueType::Chore.to_string(), "chore");
        assert_eq!(IssueType::Docs.to_string(), "docs");
        assert_eq!(IssueType::Question.to_string(), "question");
        assert_eq!(
            IssueType::Custom("my_type".to_string()).to_string(),
            "my_type"
        );
    }

    #[test]
    fn test_issue_type_as_str() {
        assert_eq!(IssueType::Task.as_str(), "task");
        assert_eq!(IssueType::Bug.as_str(), "bug");
        assert_eq!(IssueType::Feature.as_str(), "feature");
        assert_eq!(IssueType::Epic.as_str(), "epic");
        assert_eq!(IssueType::Chore.as_str(), "chore");
        assert_eq!(IssueType::Docs.as_str(), "docs");
        assert_eq!(IssueType::Question.as_str(), "question");
        assert_eq!(IssueType::Custom("custom".to_string()).as_str(), "custom");
    }

    #[test]
    fn test_issue_type_default() {
        assert_eq!(IssueType::default(), IssueType::Task);
    }

    // ========================================================================
    // DEPENDENCY TYPE TESTS
    // ========================================================================

    #[test]
    fn test_dependency_type_from_str_all_variants() {
        assert_eq!(
            DependencyType::from_str("blocks").unwrap(),
            DependencyType::Blocks
        );
        assert_eq!(
            DependencyType::from_str("parent-child").unwrap(),
            DependencyType::ParentChild
        );
        assert_eq!(
            DependencyType::from_str("conditional-blocks").unwrap(),
            DependencyType::ConditionalBlocks
        );
        assert_eq!(
            DependencyType::from_str("waits-for").unwrap(),
            DependencyType::WaitsFor
        );
        assert_eq!(
            DependencyType::from_str("related").unwrap(),
            DependencyType::Related
        );
        assert_eq!(
            DependencyType::from_str("discovered-from").unwrap(),
            DependencyType::DiscoveredFrom
        );
        assert_eq!(
            DependencyType::from_str("replies-to").unwrap(),
            DependencyType::RepliesTo
        );
        assert_eq!(
            DependencyType::from_str("relates-to").unwrap(),
            DependencyType::RelatesTo
        );
        assert_eq!(
            DependencyType::from_str("duplicates").unwrap(),
            DependencyType::Duplicates
        );
        assert_eq!(
            DependencyType::from_str("supersedes").unwrap(),
            DependencyType::Supersedes
        );
        assert_eq!(
            DependencyType::from_str("caused-by").unwrap(),
            DependencyType::CausedBy
        );
    }

    #[test]
    fn test_dependency_type_from_str_custom() {
        let result = DependencyType::from_str("my-custom-dep").unwrap();
        assert_eq!(result, DependencyType::Custom("my-custom-dep".to_string()));

        let mixed_case = DependencyType::from_str("My-Custom-Dep").unwrap();
        assert_eq!(
            mixed_case,
            DependencyType::Custom("my-custom-dep".to_string())
        );
    }

    #[test]
    fn test_dependency_type_is_blocking() {
        assert!(DependencyType::Blocks.is_blocking());
        assert!(DependencyType::ParentChild.is_blocking());
        assert!(DependencyType::ConditionalBlocks.is_blocking());
        assert!(DependencyType::WaitsFor.is_blocking());
        assert!(!DependencyType::Related.is_blocking());
        assert!(!DependencyType::DiscoveredFrom.is_blocking());
        assert!(!DependencyType::RepliesTo.is_blocking());
        assert!(!DependencyType::RelatesTo.is_blocking());
        assert!(!DependencyType::Duplicates.is_blocking());
        assert!(!DependencyType::Supersedes.is_blocking());
        assert!(!DependencyType::CausedBy.is_blocking());
        assert!(!DependencyType::Custom("custom".to_string()).is_blocking());
    }

    #[test]
    fn test_dependency_type_affects_ready_work_all() {
        assert!(DependencyType::Blocks.affects_ready_work());
        assert!(DependencyType::ParentChild.affects_ready_work());
        assert!(DependencyType::ConditionalBlocks.affects_ready_work());
        assert!(DependencyType::WaitsFor.affects_ready_work());
        assert!(!DependencyType::Related.affects_ready_work());
        assert!(!DependencyType::DiscoveredFrom.affects_ready_work());
        assert!(!DependencyType::RepliesTo.affects_ready_work());
        assert!(!DependencyType::RelatesTo.affects_ready_work());
        assert!(!DependencyType::Duplicates.affects_ready_work());
        assert!(!DependencyType::Supersedes.affects_ready_work());
        assert!(!DependencyType::CausedBy.affects_ready_work());
        assert!(!DependencyType::Custom("custom".to_string()).affects_ready_work());
    }

    #[test]
    fn test_dependency_type_display() {
        assert_eq!(DependencyType::Blocks.to_string(), "blocks");
        assert_eq!(DependencyType::ParentChild.to_string(), "parent-child");
        assert_eq!(
            DependencyType::ConditionalBlocks.to_string(),
            "conditional-blocks"
        );
        assert_eq!(DependencyType::WaitsFor.to_string(), "waits-for");
        assert_eq!(DependencyType::Related.to_string(), "related");
        assert_eq!(
            DependencyType::DiscoveredFrom.to_string(),
            "discovered-from"
        );
        assert_eq!(DependencyType::RepliesTo.to_string(), "replies-to");
        assert_eq!(DependencyType::RelatesTo.to_string(), "relates-to");
        assert_eq!(DependencyType::Duplicates.to_string(), "duplicates");
        assert_eq!(DependencyType::Supersedes.to_string(), "supersedes");
        assert_eq!(DependencyType::CausedBy.to_string(), "caused-by");
        assert_eq!(
            DependencyType::Custom("custom".to_string()).to_string(),
            "custom"
        );
    }

    // ========================================================================
    // ISSUE CONTENT HASH TESTS
    // ========================================================================

    fn create_test_issue() -> Issue {
        Issue {
            id: "bd-test".to_string(),
            content_hash: None,
            title: "Test Title".to_string(),
            description: Some("Test Description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            prerequisites: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: None,
            updated_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            closed_by: None,
            closed_by_agent_name: None,
            closed_by_harness: None,
            closed_by_model: None,
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            source_repo_path: None,
            agent_context: None,
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
    fn test_issue_content_hash_deterministic() {
        let issue1 = create_test_issue();
        let issue2 = create_test_issue();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Same content should produce same hash");
        assert!(!hash1.is_empty(), "Hash should not be empty");
    }

    #[test]
    fn test_issue_content_hash_changes_on_title_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.title = "Different Title".to_string();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different title should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_description_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.description = Some("Different Description".to_string());

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different description should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_status_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.status = Status::Closed;

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different status should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_changes_on_priority_update() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.priority = Priority::CRITICAL;

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_ne!(
            hash1, hash2,
            "Different priority should produce different hash"
        );
    }

    #[test]
    fn test_issue_content_hash_unchanged_by_timestamps() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.created_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        issue2.updated_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Different timestamps should NOT change hash");
    }

    #[test]
    fn test_issue_content_hash_unchanged_by_id() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.id = "bd-different".to_string();

        let hash1 = issue1.compute_content_hash();
        let hash2 = issue2.compute_content_hash();

        assert_eq!(hash1, hash2, "Different ID should NOT change hash");
    }

    #[test]
    fn test_issue_sync_equals_ignores_audit_timestamps_and_relation_order() {
        let mut issue1 = create_test_issue();
        issue1.labels = vec!["backend".to_string(), "bug".to_string()];
        issue1.dependencies = vec![
            Dependency {
                issue_id: issue1.id.clone(),
                depends_on_id: "bd-parent".to_string(),
                dep_type: DependencyType::Blocks,
                created_at: Utc.timestamp_opt(1_700_000_100, 0).unwrap(),
                created_by: Some("alice".to_string()),
                metadata: Some("{\"source\":\"cli\"}".to_string()),
                thread_id: Some("br-1".to_string()),
            },
            Dependency {
                issue_id: issue1.id.clone(),
                depends_on_id: "bd-epic".to_string(),
                dep_type: DependencyType::ParentChild,
                created_at: Utc.timestamp_opt(1_700_000_200, 0).unwrap(),
                created_by: Some("alice".to_string()),
                metadata: None,
                thread_id: None,
            },
        ];
        issue1.comments = vec![
            Comment {
                id: 2,
                issue_id: issue1.id.clone(),
                author: "alice".to_string(),
                body: "second".to_string(),
                created_at: Utc.timestamp_opt(1_700_000_200, 0).unwrap(),
                revision: None,
            },
            Comment {
                id: 1,
                issue_id: issue1.id.clone(),
                author: "alice".to_string(),
                body: "first".to_string(),
                created_at: Utc.timestamp_opt(1_700_000_100, 0).unwrap(),
                revision: None,
            },
        ];

        let mut issue2 = issue1.clone();
        issue2.created_at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        issue2.updated_at = Utc.timestamp_opt(1_800_000_500, 0).unwrap();
        issue2.labels.reverse();
        issue2.dependencies.reverse();
        issue2.comments.reverse();
        issue2.content_hash = Some("stale-hash".to_string());

        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));
    }

    #[test]
    fn test_issue_sync_equals_ignores_database_local_comment_ids() {
        // GitHub #486: the comment id is a per-database rowid; two clones can
        // publish the same comment under different ids (or different comments
        // under the same id) and sync must judge them by payload.
        let mut issue1 = create_test_issue();
        issue1.comments = vec![Comment {
            id: 1,
            issue_id: issue1.id.clone(),
            author: "alice".to_string(),
            body: "same payload".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_100, 0).unwrap(),
            revision: None,
        }];
        let mut issue2 = issue1.clone();
        issue2.comments[0].id = 7;
        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));

        let mut issue3 = issue1.clone();
        issue3.comments[0].body = "different payload".to_string();
        assert!(!issue1.sync_equals(&issue3));

        let mut issue4 = issue1.clone();
        issue4.comments[0].author = "bob".to_string();
        assert!(!issue1.sync_equals(&issue4));

        let mut issue5 = issue1.clone();
        issue5.comments[0].created_at = Utc.timestamp_opt(1_700_000_101, 0).unwrap();
        assert!(!issue1.sync_equals(&issue5));

        let mut issue6 = issue1.clone();
        issue6.comments.push(issue1.comments[0].clone());
        assert!(!issue1.sync_equals(&issue6), "multiplicity still matters");
    }

    #[test]
    fn test_issue_sync_equals_detects_semantic_changes() {
        let issue1 = create_test_issue();
        let mut issue2 = create_test_issue();
        issue2.due_at = Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap());

        assert!(!issue1.sync_equals(&issue2));
    }

    #[test]
    fn prerequisites_survive_serialization_and_affect_sync_equality() {
        let mut issue = create_test_issue();
        issue.acceptance_criteria = Some("- [ ] Implement behavior\n".to_owned());
        issue.prerequisites = Some("## Prepare\r\n- [X] Review the API\r\n".to_owned());
        let json = serde_json::to_value(&issue).unwrap();
        assert_eq!(
            json["prerequisites"],
            issue.prerequisites.as_deref().unwrap()
        );
        let decoded: Issue = serde_json::from_value(json).unwrap();
        assert!(issue.sync_equals(&decoded));
        assert_eq!(decoded.acceptance_criteria, issue.acceptance_criteria);
        let mut changed = decoded.clone();
        changed.prerequisites = Some("- [ ] Review the API".to_owned());
        assert!(!issue.sync_equals(&changed));
        assert!(!changed.sync_equals(&issue));
        changed.prerequisites = None;
        assert!(!issue.sync_equals(&changed));
    }

    #[test]
    fn test_issue_sync_equals_ignores_machine_local_source_repo_path() {
        // beads_rust#528: source_repo_path is machine-local and never exported,
        // so a DB row carrying it must still equal the path-free JSONL row.
        let mut issue1 = create_test_issue();
        issue1.source_repo = Some("widget_engine".to_string());
        issue1.source_repo_path = Some("/data/projects/widget_engine".to_string());

        let mut issue2 = issue1.clone();
        issue2.source_repo_path = Some("/data/projects/alternate/widget_engine".to_string());
        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));

        let mut exported = issue1.clone();
        exported.source_repo_path = None;
        assert!(issue1.sync_equals(&exported));
        assert!(exported.sync_equals(&issue1));

        // The portable source_repo name is still part of the synced payload.
        let mut renamed = issue1.clone();
        renamed.source_repo = Some("other_engine".to_string());
        assert!(!issue1.sync_equals(&renamed));
    }

    #[test]
    fn test_issue_sync_equals_detects_agent_context_changes() {
        // beads_rust#297: an agent_context-only change is a real payload
        // difference; if sync_equals ignored it, three-way merge / import
        // de-dup would silently drop a peer's inherited-context update.
        let mut issue1 = create_test_issue();
        issue1.agent_context = Some(r#"{"constraints":["no network"]}"#.to_string());

        let mut issue2 = issue1.clone();
        issue2.agent_context = Some(r#"{"constraints":["offline only"]}"#.to_string());
        assert!(!issue1.sync_equals(&issue2));
        assert!(!issue2.sync_equals(&issue1));

        // Setting vs. clearing the field is also a difference.
        let mut issue3 = issue1.clone();
        issue3.agent_context = None;
        assert!(!issue1.sync_equals(&issue3));
        assert!(!issue3.sync_equals(&issue1));

        // Identical agent_context still compares equal.
        let issue4 = issue1.clone();
        assert!(issue1.sync_equals(&issue4));
    }

    #[test]
    fn test_issue_sync_equals_treats_duplicate_labels_as_equivalent() {
        let mut issue1 = create_test_issue();
        issue1.labels = vec![
            "backend".to_string(),
            "backend".to_string(),
            "urgent".to_string(),
        ];

        let mut issue2 = create_test_issue();
        issue2.labels = vec!["urgent".to_string(), "backend".to_string()];

        assert!(issue1.sync_equals(&issue2));
        assert!(issue2.sync_equals(&issue1));
    }

    // ========================================================================
    // ISSUE TOMBSTONE TESTS
    // ========================================================================

    #[test]
    fn test_is_expired_tombstone_not_tombstone() {
        let issue = create_test_issue();
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_no_retention() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(100));
        assert!(!issue.is_expired_tombstone(None));
    }

    #[test]
    fn test_is_expired_tombstone_zero_retention() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(100));
        assert!(!issue.is_expired_tombstone(Some(0)));
    }

    #[test]
    fn test_is_expired_tombstone_no_deleted_at() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_not_expired() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(10));
        assert!(!issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_expired() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc::now() - chrono::Duration::days(40));
        assert!(issue.is_expired_tombstone(Some(30)));
    }

    #[test]
    fn test_is_expired_tombstone_at_uses_strict_ttl_boundary() {
        let as_of = DateTime::parse_from_rfc3339("2026-07-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(as_of - chrono::Duration::days(30));

        assert!(
            !issue.is_expired_tombstone_at(Some(30), as_of),
            "a tombstone is retained at the exact TTL boundary"
        );
        assert!(
            issue.is_expired_tombstone_at(Some(30), as_of + chrono::Duration::nanoseconds(1)),
            "a tombstone expires immediately after the strict TTL boundary"
        );
    }

    #[test]
    fn test_is_expired_tombstone_at_retains_unrepresentable_expiration() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(DateTime::<Utc>::MAX_UTC);

        assert!(
            !issue.is_expired_tombstone_at(Some(1), DateTime::<Utc>::MAX_UTC),
            "an expiration beyond chrono's maximum timestamp must retain the tombstone"
        );
    }

    #[test]
    fn test_is_expired_tombstone_at_clamps_maximum_retention() {
        let deleted_at = DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clamped_expiration = deleted_at + chrono::Duration::days(365_000);
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(deleted_at);

        assert!(
            !issue.is_expired_tombstone_at(Some(u64::MAX), clamped_expiration),
            "maximum retention is clamped and remains live at the strict boundary"
        );
        assert!(
            issue.is_expired_tombstone_at(
                Some(u64::MAX),
                clamped_expiration + chrono::Duration::nanoseconds(1),
            ),
            "maximum retention expires immediately after the clamped boundary"
        );
    }

    // ========================================================================
    // EVENT TYPE TESTS
    // ========================================================================

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(EventType::Created.as_str(), "created");
        assert_eq!(EventType::Updated.as_str(), "updated");
        assert_eq!(EventType::StatusChanged.as_str(), "status_changed");
        assert_eq!(EventType::PriorityChanged.as_str(), "priority_changed");
        assert_eq!(EventType::AssigneeChanged.as_str(), "assignee_changed");
        assert_eq!(EventType::Commented.as_str(), "commented");
        assert_eq!(EventType::Closed.as_str(), "closed");
        assert_eq!(EventType::Reopened.as_str(), "reopened");
        assert_eq!(EventType::DependencyAdded.as_str(), "dependency_added");
        assert_eq!(EventType::DependencyRemoved.as_str(), "dependency_removed");
        assert_eq!(EventType::LabelAdded.as_str(), "label_added");
        assert_eq!(EventType::LabelRemoved.as_str(), "label_removed");
        assert_eq!(EventType::Compacted.as_str(), "compacted");
        assert_eq!(EventType::Deleted.as_str(), "deleted");
        assert_eq!(EventType::Restored.as_str(), "restored");
        assert_eq!(
            EventType::Custom("my_event".to_string()).as_str(),
            "my_event"
        );
    }

    #[test]
    fn test_event_type_deserialize_all() {
        let events = [
            ("\"created\"", EventType::Created),
            ("\"updated\"", EventType::Updated),
            ("\"status_changed\"", EventType::StatusChanged),
            ("\"priority_changed\"", EventType::PriorityChanged),
            ("\"assignee_changed\"", EventType::AssigneeChanged),
            ("\"commented\"", EventType::Commented),
            ("\"closed\"", EventType::Closed),
            ("\"reopened\"", EventType::Reopened),
            ("\"dependency_added\"", EventType::DependencyAdded),
            ("\"dependency_removed\"", EventType::DependencyRemoved),
            ("\"label_added\"", EventType::LabelAdded),
            ("\"label_removed\"", EventType::LabelRemoved),
            ("\"compacted\"", EventType::Compacted),
            ("\"deleted\"", EventType::Deleted),
            ("\"restored\"", EventType::Restored),
        ];

        for (json, expected) in events {
            let result: EventType = serde_json::from_str(json).unwrap();
            assert_eq!(result, expected, "Failed to deserialize {json}");
        }
    }

    #[test]
    fn test_event_type_deserialize_custom() {
        let result: EventType = serde_json::from_str("\"my_custom_event\"").unwrap();
        assert_eq!(result, EventType::Custom("my_custom_event".to_string()));

        let mixed_case: EventType = serde_json::from_str("\"My_Custom_Event\"").unwrap();
        assert_eq!(mixed_case, EventType::Custom("my_custom_event".to_string()));
    }

    #[test]
    fn test_event_type_deserialize_normalizes_case() {
        let result: EventType = serde_json::from_str("\"CREATED\"").unwrap();
        assert_eq!(result, EventType::Created);

        let result: EventType = serde_json::from_str("\"Status_Changed\"").unwrap();
        assert_eq!(result, EventType::StatusChanged);
    }

    // ========================================================================
    // COMMENT TESTS
    // ========================================================================

    fn legacy_comment(id: i64, body: &str) -> Comment {
        Comment {
            id,
            issue_id: "bd-comment".to_owned(),
            author: "alice".to_owned(),
            body: body.to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
            revision: None,
        }
    }

    fn enrolled_comment() -> Comment {
        let mut comments = vec![legacy_comment(7, "private-original")];
        normalize_comment_revisions(&mut comments).unwrap();
        comments.remove(0)
    }

    #[test]
    fn comment_revision_sequential_edits_keep_identity_and_causal_history() {
        let original = enrolled_comment();
        let mut first = original.clone();
        first
            .revise(
                CommentState::Active,
                "corrected",
                "alice",
                original.created_at,
            )
            .unwrap();
        let mut second = first.clone();
        second
            .revise(
                CommentState::Active,
                "final",
                "alice",
                original.created_at - chrono::Duration::days(1),
            )
            .unwrap();
        let roundtrip: Comment =
            serde_json::from_str(&serde_json::to_string(&second).unwrap()).unwrap();
        let merged = merge_comment_lists(
            std::slice::from_ref(&original),
            &[roundtrip],
            &original.issue_id,
        )
        .unwrap();
        assert_eq!(merged[0].body, "final");
        assert_eq!(merged[0].id, original.id);
        assert_eq!(merged[0].author, original.author);
        assert_eq!(merged[0].created_at, original.created_at);
        assert_eq!(
            merged[0].revision.as_ref().unwrap().identity,
            original.revision.as_ref().unwrap().identity
        );
        assert!(
            merged[0]
                .revision
                .as_ref()
                .unwrap()
                .ancestors
                .contains(&first.revision.as_ref().unwrap().head)
        );
        assert!(
            !serde_json::to_string(&merged)
                .unwrap()
                .contains("private-original")
        );
    }

    #[test]
    fn comment_revision_concurrent_edits_fail_even_with_unequal_generations() {
        let origin = enrolled_comment();
        let mut left = origin.clone();
        let mut right = origin.clone();
        left.revise(
            CommentState::Active,
            "left-secret",
            "alice",
            origin.created_at,
        )
        .unwrap();
        right
            .revise(
                CommentState::Active,
                "right-secret",
                "alice",
                origin.created_at,
            )
            .unwrap();
        for _ in 0..2 {
            let error = merge_comment_lists(
                std::slice::from_ref(&left),
                std::slice::from_ref(&right),
                &origin.issue_id,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("Concurrent edits"));
            assert!(!error.contains("left-secret") && !error.contains("right-secret"));
            assert!(
                merge_comment_lists(
                    std::slice::from_ref(&right),
                    std::slice::from_ref(&left),
                    &origin.issue_id
                )
                .is_err()
            );
            left.revise(
                CommentState::Active,
                "left-followup",
                "alice",
                origin.created_at,
            )
            .unwrap();
        }
    }

    #[test]
    fn comment_revision_redaction_dominates_future_edits_and_repeated_legacy_imports() {
        let legacy = legacy_comment(999, "private-original");
        let origin = enrolled_comment();
        let mut redacted = origin.clone();
        redacted
            .revise(CommentState::Redacted, "", "moderator", origin.created_at)
            .unwrap();
        let mut future = origin.clone();
        for body in ["branch-one", "branch-two", "branch-three"] {
            future
                .revise(
                    CommentState::Active,
                    body,
                    "alice",
                    origin.created_at + chrono::Duration::days(365),
                )
                .unwrap();
        }
        for (left, right) in [(&redacted, &future), (&future, &redacted)] {
            let mut result = merge_comment_lists(
                std::slice::from_ref(left),
                std::slice::from_ref(right),
                &origin.issue_id,
            )
            .unwrap();
            assert_eq!(result[0].state(), CommentState::Redacted);
            assert!(result[0].body.is_empty());
            for _ in 0..3 {
                let again =
                    merge_comment_lists(&result, std::slice::from_ref(&legacy), &origin.issue_id)
                        .unwrap();
                assert!(comments_sync_equal(&result, &again));
                result = again;
            }
            let serialized = serde_json::to_string(&result).unwrap();
            assert!(!serialized.contains("private-original") && !serialized.contains("branch-"));
            assert_eq!(result[0].revision.as_ref().unwrap().updated_by, "moderator");
        }
    }

    #[test]
    fn comment_revision_delete_dominates_redaction_in_every_order() {
        let origin = enrolled_comment();
        let mut redacted = origin.clone();
        let mut deleted = origin.clone();
        redacted
            .revise(
                CommentState::Redacted,
                "",
                "alice",
                origin.created_at + chrono::Duration::days(3),
            )
            .unwrap();
        deleted
            .revise(CommentState::Deleted, "", "alice", origin.created_at)
            .unwrap();
        let left = merge_comment_lists(
            std::slice::from_ref(&deleted),
            std::slice::from_ref(&redacted),
            &origin.issue_id,
        )
        .unwrap();
        let right = merge_comment_lists(
            &[redacted],
            std::slice::from_ref(&deleted),
            &origin.issue_id,
        )
        .unwrap();
        assert!(comments_sync_equal(&left, &right));
        assert_eq!(left[0].state(), CommentState::Deleted);
        assert!(
            deleted
                .revise(CommentState::Active, "restore", "alice", origin.created_at)
                .is_err()
        );
        assert!(
            deleted
                .revise(CommentState::Redacted, "", "alice", origin.created_at)
                .is_err()
        );
        let before = deleted.clone();
        deleted
            .revise(CommentState::Deleted, "", "alice", origin.created_at)
            .unwrap();
        assert_eq!(before, deleted, "repeated removal is idempotent");
    }

    #[test]
    fn comment_revision_preserves_legacy_multiplicity_and_local_id_collisions() {
        let legacy = vec![legacy_comment(3, "same"), legacy_comment(4, "same")];
        let mut enrolled = legacy.clone();
        normalize_comment_revisions(&mut enrolled).unwrap();
        assert_ne!(
            enrolled[0].revision.as_ref().unwrap().identity,
            enrolled[1].revision.as_ref().unwrap().identity
        );
        let timestamp = enrolled[1].created_at;
        enrolled[0]
            .revise(CommentState::Redacted, "", "alice", timestamp)
            .unwrap();
        let merged = merge_comment_lists(&enrolled, &legacy, "bd-comment").unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged.iter().filter(|c| c.body == "same").count(), 1);
        assert_eq!(
            merged
                .iter()
                .filter(|c| c.state() == CommentState::Redacted)
                .count(),
            1
        );
        let collision = merge_comment_lists(
            &[legacy_comment(1, "one")],
            &[legacy_comment(1, "two")],
            "bd-comment",
        )
        .unwrap();
        assert_eq!(collision.len(), 2);
        assert_eq!(collision.iter().filter(|c| c.id == 0).count(), 1);
    }

    #[test]
    fn comment_revision_refuses_ambiguous_mixed_enrollment_without_restoring_text() {
        let original = legacy_comment(7, "private-original");
        let mut enrolled = vec![original.clone()];
        normalize_comment_revisions(&mut enrolled).unwrap();
        let mut legacy_copy = original;
        legacy_copy.id = 8;
        let error = normalize_comment_revisions(&mut [enrolled[0].clone(), legacy_copy.clone()])
            .unwrap_err();
        assert!(!error.to_string().contains("private-original"));
        let now = enrolled[0].created_at;
        enrolled[0]
            .revise(CommentState::Redacted, "", "alice", now)
            .unwrap();
        assert!(
            normalize_comment_revisions(&mut [enrolled[0].clone(), legacy_copy.clone()]).is_err()
        );
        // Separate complete snapshots remain safe and idempotent.
        let merged = merge_comment_lists(&enrolled, &[legacy_copy], "bd-comment").unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].state(), CommentState::Redacted);
        assert!(merged[0].body.is_empty());
    }

    #[test]
    fn comment_revision_older_peer_cannot_steal_local_numeric_target() {
        let local = legacy_comment(7, "local");
        let mut remote = legacy_comment(7, "remote");
        remote.created_at -= chrono::Duration::seconds(1);
        let merged = merge_comment_lists(
            std::slice::from_ref(&local),
            std::slice::from_ref(&remote),
            &local.issue_id,
        )
        .unwrap();
        assert_eq!(merged[0].body, "remote");
        assert_eq!(
            merged[0].id, 0,
            "only the imported collision needs allocation"
        );
        assert_eq!(merged[1].body, "local");
        assert_eq!(merged[1].id, 7);
        let again = merge_comment_lists(&merged, &[remote], &local.issue_id).unwrap();
        assert_eq!(again, merged);
    }

    #[test]
    fn comment_revision_terminals_make_multi_branch_join_associative() {
        let origin = enrolled_comment();
        let mut edit = origin.clone();
        let mut redact = origin.clone();
        let mut delete = origin.clone();
        edit.revise(CommentState::Active, "changed", "alice", origin.created_at)
            .unwrap();
        redact
            .revise(CommentState::Redacted, "", "alice", origin.created_at)
            .unwrap();
        delete
            .revise(CommentState::Deleted, "", "alice", origin.created_at)
            .unwrap();
        let branches = [edit, redact, delete];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let first = merge_comment_lists(
                std::slice::from_ref(&branches[order[0]]),
                std::slice::from_ref(&branches[order[1]]),
                &origin.issue_id,
            )
            .unwrap();
            let result = merge_comment_lists(
                &first,
                std::slice::from_ref(&branches[order[2]]),
                &origin.issue_id,
            )
            .unwrap();
            assert!(comments_sync_equal(
                &result,
                std::slice::from_ref(&branches[2])
            ));
        }
    }

    #[test]
    fn comment_revision_rejects_malformed_and_attribution_changes_without_text_in_errors() {
        let origin = enrolled_comment();
        let mut malformed = origin.clone();
        malformed.revision.as_mut().unwrap().head = "not-a-revision".to_owned();
        assert!(normalize_comment_revisions(&mut [malformed]).is_err());
        let mut forged = origin.clone();
        forged.author = "mallory".to_owned();
        assert!(
            merge_comment_lists(std::slice::from_ref(&origin), &[forged], &origin.issue_id)
                .is_err()
        );
        let mut leaked = origin.clone();
        leaked
            .revise(CommentState::Redacted, "", "alice", origin.created_at)
            .unwrap();
        leaked.body = "private-leak".to_owned();
        let error = leaked.validate_revision().unwrap_err().to_string();
        assert!(!error.contains("private-leak"));
    }

    #[test]
    fn comment_revision_semantic_equality_detects_metadata_and_ignores_rowids() {
        let origin = enrolled_comment();
        let mut copy = origin.clone();
        copy.id = 500;
        assert!(comments_sync_equal(
            std::slice::from_ref(&origin),
            std::slice::from_ref(&copy)
        ));
        copy.revise(CommentState::Active, "changed", "alice", origin.created_at)
            .unwrap();
        assert!(!comments_sync_equal(&[origin], &[copy]));
    }

    #[test]
    fn comment_revision_privacy_removal_remains_available_at_edit_limit() {
        let mut comment = enrolled_comment();
        let now = comment.created_at;
        let revision = comment.revision.as_mut().unwrap();
        revision.generation = u64::MAX;
        revision.head = "f".repeat(64);
        revision.ancestors = vec![revision.identity.clone()];
        assert!(
            comment
                .revise(CommentState::Active, "edit", "alice", now)
                .is_err()
        );
        comment
            .revise(CommentState::Redacted, "", "alice", now)
            .unwrap();
        assert_eq!(comment.state(), CommentState::Redacted);
        assert!(comment.body.is_empty());
    }

    #[test]
    fn comment_revision_privacy_markers_prevent_automatic_parent_expiration() {
        let mut issue = create_test_issue();
        issue.status = Status::Tombstone;
        issue.deleted_at = Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap());
        let mut comment = enrolled_comment();
        let now = comment.created_at;
        assert!(issue.is_expired_tombstone_at(Some(1), now));
        comment
            .revise(CommentState::Redacted, "", "alice", now)
            .unwrap();
        issue.comments.push(comment);
        assert!(!issue.is_expired_tombstone_at(Some(1), now));
    }

    #[test]
    fn test_comment_serialization_roundtrip() {
        let comment = Comment {
            id: 123,
            issue_id: "bd-abc".to_string(),
            author: "testuser".to_string(),
            body: "This is a comment".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            revision: None,
        };

        let json = serde_json::to_string(&comment).unwrap();
        let deserialized: Comment = serde_json::from_str(&json).unwrap();

        assert_eq!(comment, deserialized);
    }

    #[test]
    fn test_comment_text_field_renamed() {
        let json = r#"{"id":1,"issue_id":"bd-123","author":"user","text":"comment body","created_at":"2026-01-01T00:00:00Z"}"#;
        let comment: Comment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.body, "comment body");
    }

    // ========================================================================
    // DEPENDENCY TESTS
    // ========================================================================

    #[test]
    fn test_dependency_serialization_roundtrip() {
        let dep = Dependency {
            issue_id: "bd-abc".to_string(),
            depends_on_id: "bd-xyz".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: Some("testuser".to_string()),
            metadata: None,
            thread_id: None,
        };

        let json = serde_json::to_string(&dep).unwrap();
        let deserialized: Dependency = serde_json::from_str(&json).unwrap();

        assert_eq!(dep, deserialized);
    }

    #[test]
    fn test_dependency_type_field_renamed() {
        let json = r#"{"issue_id":"bd-1","depends_on_id":"bd-2","type":"blocks","created_at":"2026-01-01T00:00:00Z"}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.dep_type, DependencyType::Blocks);
    }

    #[test]
    fn dependency_empty_string_metadata_coerced_to_none() {
        // Regression for issue #323: legacy JSONL serialized absent dependency
        // metadata as `"metadata":""`, which is not valid JSON. Deserialization
        // must tolerate it by coercing the degenerate empty string to `None`
        // (lossless: the import path materializes `None` as `"{}"`).
        let json = r#"{
            "issue_id": "example-1",
            "depends_on_id": "example-0",
            "type": "blocks",
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "user",
            "metadata": "",
            "thread_id": ""
        }"#;
        let dep: Dependency = serde_json::from_str(json).expect("empty metadata must deserialize");
        assert_eq!(
            dep.metadata, None,
            "empty-string metadata should become None"
        );
        // thread_id intentionally has no coercion: only `metadata` is parsed as
        // JSON downstream, so only it needs the empty-string tolerance.
    }

    #[test]
    fn dependency_whitespace_metadata_coerced_to_none() {
        let json = r#"{"issue_id":"a","depends_on_id":"b","type":"blocks","created_at":"2026-01-01T00:00:00Z","metadata":"   "}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.metadata, None);
    }

    #[test]
    fn dependency_real_metadata_preserved() {
        // Genuine JSON metadata must round-trip unchanged (no data loss).
        let json = r#"{"issue_id":"a","depends_on_id":"b","type":"blocks","created_at":"2026-01-01T00:00:00Z","metadata":"{\"k\":\"v\"}"}"#;
        let dep: Dependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.metadata.as_deref(), Some(r#"{"k":"v"}"#));
        // And it survives a serialize round-trip.
        let reser = serde_json::to_string(&dep).unwrap();
        let dep2: Dependency = serde_json::from_str(&reser).unwrap();
        assert_eq!(dep, dep2);
    }

    // ========================================================================
    // EVENT TESTS
    // ========================================================================

    #[test]
    fn test_event_serialization_roundtrip() {
        let event = Event {
            id: 456,
            issue_id: "bd-abc".to_string(),
            event_type: EventType::StatusChanged,
            actor: "testuser".to_string(),
            old_value: Some("open".to_string()),
            new_value: Some("closed".to_string()),
            comment: None,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            agent_name: Some("agent-1".to_string()),
            harness: Some("codex-cli".to_string()),
            model: Some("opus-4".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: Event = serde_json::from_str(&json).unwrap();

        assert_eq!(event, deserialized);
    }

    // ========================================================================
    // EPIC STATUS TESTS
    // ========================================================================

    #[test]
    fn test_epic_status_serialization() {
        let epic_status = EpicStatus {
            epic: create_test_issue(),
            total_children: 10,
            closed_children: 7,
            eligible_for_close: false,
        };

        let json = serde_json::to_string(&epic_status).unwrap();
        assert!(json.contains("\"total_children\":10"));
        assert!(json.contains("\"closed_children\":7"));
        assert!(json.contains("\"eligible_for_close\":false"));
    }
}
