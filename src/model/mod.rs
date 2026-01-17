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
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
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
            "docs" => Ok(Self::Docs),
            "question" => Ok(Self::Question),
            other => Ok(Self::Custom(other.to_string())),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

impl Issue {
    /// Compute the deterministic content hash for this issue.
    ///
    /// Includes: title, description, design, `acceptance_criteria`, notes,
    /// status, priority, `issue_type`, assignee, `external_ref`, pinned, `is_template`.
    /// Excludes: id, timestamps, relations, tombstones.
    #[must_use]
    pub fn compute_content_hash(&self) -> String {
        let mut hasher = Sha256::new();

        // Helper to update with null separator
        let mut update = |s: &str| {
            hasher.update(s.as_bytes());
            hasher.update([0]);
        };

        update(&self.title);
        update(self.description.as_deref().unwrap_or(""));
        update(self.design.as_deref().unwrap_or(""));
        update(self.acceptance_criteria.as_deref().unwrap_or(""));
        update(self.notes.as_deref().unwrap_or(""));
        update(self.status.as_str());
        update(&self.priority.0.to_string());
        update(self.issue_type.as_str());
        update(self.assignee.as_deref().unwrap_or(""));
        update(self.external_ref.as_deref().unwrap_or(""));
        update(if self.pinned { "true" } else { "false" });
        // Last one doesn't strictly need a separator if it's the end, but for consistency/extensibility:
        hasher.update((if self.is_template { "true" } else { "false" }).as_bytes());

        format!("{:x}", hasher.finalize())
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
}
