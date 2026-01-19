//! Core data types for `beads_rust`.
//!
//! This module defines the fundamental types used throughout the application:
//! - `Issue` - The core work item
//! - `Status` - Issue lifecycle states
//! - `IssueType` - Categories of issues
//! - `Dependency` - Relationships between issues
//! - `Comment` - Issue comments
//! - `Event` - Audit log entries

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
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

/// Issue lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Blocked,
    Deferred,
    Closed,
    #[serde(rename = "tombstone")]
    Tombstone,
    #[serde(rename = "pinned")]
    Pinned,
    #[serde(untagged)]
    Custom(String),
}

impl Status {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
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
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for Status {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "open" => Ok(Self::Open),
            "in_progress" | "inprogress" => Ok(Self::InProgress),
            "blocked" => Ok(Self::Blocked),
            "deferred" => Ok(Self::Deferred),
            "closed" => Ok(Self::Closed),
            "tombstone" => Ok(Self::Tombstone),
            "pinned" => Ok(Self::Pinned),
            other => Err(crate::error::BeadsError::InvalidStatus {
                status: other.to_string(),
            }),
        }
    }
}

/// Issue priority (0=Critical, 4=Backlog).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(transparent)]
pub struct Priority(pub i32);

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
            _ => Err(crate::error::BeadsError::InvalidPriority {
                priority: val.parse().unwrap_or(-1),
            }),
        }
    }
}

/// Issue type category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
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

impl IssueType {
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
}

impl fmt::Display for IssueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for IssueType {
    type Err = crate::error::BeadsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "task" => Ok(Self::Task),
            "bug" => Ok(Self::Bug),
            "feature" => Ok(Self::Feature),
            "epic" => Ok(Self::Epic),
            "chore" => Ok(Self::Chore),
            // Note: docs and question are supported in br but not in bd
            // For bd conformance, we only accept the 5 types above via CLI
            // However, docs/question may exist in imported data
            other => Err(crate::error::BeadsError::InvalidType {
                issue_type: other.to_string(),
            }),
        }
    }
}

/// Dependency relationship type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            Self::Blocks | Self::ParentChild | Self::ConditionalBlocks
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
        let event_type = match value.as_str() {
            "created" => Self::Created,
            "updated" => Self::Updated,
            "status_changed" => Self::StatusChanged,
            "priority_changed" => Self::PriorityChanged,
            "assignee_changed" => Self::AssigneeChanged,
            "commented" => Self::Commented,
            "closed" => Self::Closed,
            "reopened" => Self::Reopened,
            "dependency_added" => Self::DependencyAdded,
            "dependency_removed" => Self::DependencyRemoved,
            "label_added" => Self::LabelAdded,
            "label_removed" => Self::LabelRemoved,
            "compacted" => Self::Compacted,
            "deleted" => Self::Deleted,
            "restored" => Self::Restored,
            _ => Self::Custom(value),
        };
        Ok(event_type)
    }
}

/// The primary issue entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
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
    /// Includes: title, description, design, `acceptance_criteria`, notes,
    /// status, priority, `issue_type`, assignee, `external_ref`, pinned, `is_template`.
    /// Excludes: id, timestamps, relations, tombstones.
    ///
    /// Delegates to [`crate::util::hash::content_hash`] for the canonical implementation.
    #[must_use]
    pub fn compute_content_hash(&self) -> String {
        crate::util::content_hash(self)
    }

    /// Check if this issue is a tombstone that has exceeded its TTL.
    #[must_use]
    pub fn is_expired_tombstone(&self, retention_days: Option<u64>) -> bool {
        if self.status != Status::Tombstone {
            return false;
        }

        let Some(days) = retention_days else {
            return false;
        };

        if days == 0 {
            return false; // Keep forever if 0 (though usually means disabled/immediate, assume safe default)
        }

        let Some(deleted_at) = self.deleted_at else {
            return false; // Keep if deletion time is unknown
        };

        let days_i64 = i64::try_from(days).unwrap_or(i64::MAX);
        let expiration_time = deleted_at + chrono::Duration::days(days_i64);
        Utc::now() > expiration_time
    }
}

/// Epic completion status with child counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpicStatus {
    pub epic: Issue,
    pub total_children: usize,
    pub closed_children: usize,
    pub eligible_for_close: bool,
}

/// Relationship between two issues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,

    /// Thread ID for conversation linking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A comment on an issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    pub issue_id: String,
    pub author: String,
    #[serde(rename = "text")]
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// An event in the issue's history (audit log).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
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
    fn test_status_from_str_invalid() {
        let result = Status::from_str("invalid_status");
        assert!(result.is_err());
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
        assert_eq!(p, Priority(0));
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
        // docs and question are rejected via FromStr for bd conformance
        // but they can still exist via serde deserialization from imports
        assert!(IssueType::from_str("docs").is_err());
        assert!(IssueType::from_str("question").is_err());
    }

    #[test]
    fn test_issue_type_from_str_case_insensitive() {
        assert_eq!(IssueType::from_str("TASK").unwrap(), IssueType::Task);
        assert_eq!(IssueType::from_str("BUG").unwrap(), IssueType::Bug);
        assert_eq!(IssueType::from_str("Feature").unwrap(), IssueType::Feature);
    }

    #[test]
    fn test_issue_type_from_str_custom_rejected() {
        // Custom/unknown types are rejected for bd conformance
        let result = IssueType::from_str("custom_type");
        assert!(result.is_err());
        // IssueType::Custom can still exist via serde deserialization
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
    }

    #[test]
    fn test_dependency_type_is_blocking() {
        assert!(DependencyType::Blocks.is_blocking());
        assert!(DependencyType::ParentChild.is_blocking());
        assert!(DependencyType::ConditionalBlocks.is_blocking());
        assert!(!DependencyType::WaitsFor.is_blocking());
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
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
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
    }

    // ========================================================================
    // COMMENT TESTS
    // ========================================================================

    #[test]
    fn test_comment_serialization_roundtrip() {
        let comment = Comment {
            id: 123,
            issue_id: "bd-abc".to_string(),
            author: "testuser".to_string(),
            body: "This is a comment".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
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
