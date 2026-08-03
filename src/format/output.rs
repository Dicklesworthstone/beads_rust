use crate::model::{Comment, Event, Issue, IssueType, Priority, Status};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Minimal issue output for stale command (bd parity).
/// Contains only the fields that bd's stale command outputs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StaleIssue {
    pub created_at: DateTime<Utc>,
    pub id: String,
    pub issue_type: IssueType,
    pub priority: Priority,
    pub status: Status,
    pub title: String,
    pub updated_at: DateTime<Utc>,
}

/// Minimal issue output for ready command (bd parity).
///
/// Contains only the fields that bd's ready command outputs.
/// Does NOT include: `compaction_level`, `original_size`, `dependency_count`, `dependent_count`
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadyIssue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<i32>,
    pub id: String,
    pub issue_type: IssueType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub priority: Priority,
    pub status: Status,
    pub title: String,
    pub updated_at: DateTime<Utc>,
}

impl From<&Issue> for ReadyIssue {
    fn from(issue: &Issue) -> Self {
        Self {
            acceptance_criteria: issue.acceptance_criteria.clone(),
            assignee: issue.assignee.clone(),
            created_at: issue.created_at,
            created_by: issue.created_by.clone(),
            description: issue.description.clone(),
            estimated_minutes: issue.estimated_minutes,
            id: issue.id.clone(),
            issue_type: issue.issue_type.clone(),
            notes: issue.notes.clone(),
            owner: issue.owner.clone(),
            priority: issue.priority,
            status: issue.status.clone(),
            title: issue.title.clone(),
            updated_at: issue.updated_at,
        }
    }
}

/// Minimal issue output for blocked command (bd parity).
///
/// Contains only the fields that bd's blocked command outputs, plus `blocked_by` info.
/// Does NOT include: `compaction_level`, `original_size`
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BlockedIssueOutput {
    pub blocked_by: Vec<String>,
    pub blocked_by_count: usize,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub id: String,
    pub issue_type: IssueType,
    pub priority: Priority,
    pub status: Status,
    pub title: String,
    pub updated_at: DateTime<Utc>,
}

impl From<&Issue> for StaleIssue {
    fn from(issue: &Issue) -> Self {
        Self {
            created_at: issue.created_at,
            id: issue.id.clone(),
            issue_type: issue.issue_type.clone(),
            priority: issue.priority,
            status: issue.status.clone(),
            title: issue.title.clone(),
            updated_at: issue.updated_at,
        }
    }
}

/// Issue with counts for list/search views.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct IssueWithCounts {
    #[serde(flatten)]
    pub issue: Issue,
    pub dependency_count: usize,
    pub dependent_count: usize,
    /// How many comments the issue has. Omitted when zero, so listings of
    /// uncommented issues keep exactly the shape they had before comments
    /// were surfaced here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub comment_count: usize,
    /// When the newest comment was written — the "is this history fresh?"
    /// signal. Never a body: listings carry counts and ages only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_comment_at: Option<DateTime<Utc>>,
    /// True when the search query matched this issue's comment text.
    ///
    /// Only ever set by `bd search`, and only when a comment matched, so a
    /// consumer can tell a comment-sourced hit from a title/description
    /// hit instead of being left to guess why an issue was returned.
    #[serde(default, skip_serializing_if = "is_false")]
    pub comment_match: bool,
}

/// Issue details with full relations for show view.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct IssueDetails {
    #[serde(flatten)]
    pub issue: Issue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<IssueWithDependencyMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependents: Vec<IssueWithDependencyMetadata>,
    /// The comments carried in this view — possibly a bounded window.
    ///
    /// `bd show` renders only the newest few (see the comments command's
    /// display bound); when it does, `comment_count` still reports the
    /// true total and `comments_truncated` is set. Consumers that need
    /// every comment ask `bd comments <id>`, and export never comes
    /// through here at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    /// Total comments on the issue, independent of how many are in
    /// `comments`. Present whenever the issue has any, so a machine
    /// consumer can always tell "no comments" from "comments not shown".
    #[serde(default, skip_serializing_if = "is_zero")]
    pub comment_count: usize,
    /// True when `comments` is a bounded window over a longer log — the
    /// explicit signal that makes truncation impossible to miss.
    #[serde(default, skip_serializing_if = "is_false")]
    pub comments_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// `skip_serializing_if` helper: omit zero counts.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// `skip_serializing_if` helper: omit false flags.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IssueWithDependencyMetadata {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub priority: Priority,
    #[serde(rename = "dependency_type")]
    pub dep_type: String,
}

/// Blocked issue for blocked view.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BlockedIssue {
    #[serde(flatten)]
    pub issue: Issue,
    pub blocked_by_count: usize,
    pub blocked_by: Vec<String>,
}

/// Tree node for dependency tree view.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TreeNode {
    #[serde(flatten)]
    pub issue: Issue,
    pub depth: usize,
    pub parent_id: Option<String>,
    pub truncated: bool,
}

/// Summary statistics for the project.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatsSummary {
    pub total_issues: usize,
    pub open_issues: usize,
    pub in_progress_issues: usize,
    pub closed_issues: usize,
    pub blocked_issues: usize,
    pub deferred_issues: usize,
    pub ready_issues: usize,
    pub tombstone_issues: usize,
    pub pinned_issues: usize,
    pub epics_eligible_for_closure: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_lead_time_hours: Option<f64>,
}

/// Breakdown statistics by a dimension.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Breakdown {
    pub dimension: String,
    pub counts: Vec<BreakdownEntry>,
}

/// A single entry in a breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BreakdownEntry {
    pub key: String,
    pub count: usize,
}

/// Recent activity statistics from git history.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecentActivity {
    pub hours_tracked: u32,
    pub commit_count: usize,
    pub issues_created: usize,
    pub issues_closed: usize,
    pub issues_updated: usize,
    pub issues_reopened: usize,
    pub total_changes: usize,
}

/// Aggregate statistics output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Statistics {
    pub summary: StatsSummary,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breakdowns: Vec<Breakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_activity: Option<RecentActivity>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn base_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
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
    fn issue_with_counts_serializes_counts() {
        let issue = base_issue("bd-1", "Test");
        let iwc = IssueWithCounts {
            issue,
            dependency_count: 2,
            dependent_count: 1,
            ..Default::default()
        };

        let json = serde_json::to_string(&iwc).unwrap();
        assert!(json.contains("\"dependency_count\":2"));
        assert!(json.contains("\"dependent_count\":1"));
        assert!(json.contains("\"id\":\"bd-1\""));
    }

    /// An issue with no comments says nothing about comments: the fields are
    /// omitted rather than emitted as zeroes, so listings of thousands of
    /// comment-free issues do not grow a per-row tax.
    #[test]
    fn issue_with_counts_omits_empty_comment_fields() {
        let iwc = IssueWithCounts {
            issue: base_issue("bd-1", "Test"),
            ..Default::default()
        };

        let json = serde_json::to_string(&iwc).unwrap();
        assert!(!json.contains("comment_count"));
        assert!(!json.contains("last_comment_at"));
        assert!(!json.contains("comment_match"));
    }

    /// When comments DO exist, the count and recency are present — a reader
    /// (or an agent) can tell there is history without fetching bodies.
    #[test]
    fn issue_with_counts_serializes_comment_facts() {
        let iwc = IssueWithCounts {
            issue: base_issue("bd-1", "Test"),
            comment_count: 3,
            last_comment_at: Some(Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0).unwrap()),
            comment_match: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&iwc).unwrap();
        assert!(json.contains("\"comment_count\":3"));
        assert!(json.contains("last_comment_at"));
        assert!(json.contains("\"comment_match\":true"));
    }

    #[test]
    fn issue_details_serializes_parent_and_relations() {
        let issue = base_issue("bd-2", "Details");
        let details = IssueDetails {
            issue,
            labels: vec!["backend".to_string()],
            dependencies: vec![],
            dependents: vec![],
            comments: vec![],
            events: vec![],
            parent: Some("bd-parent".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"parent\":\"bd-parent\""));
        assert!(json.contains("\"labels\":[\"backend\"]"));
    }

    /// The JSON contract for a bounded comment list: consumers must be able
    /// to detect that they are holding a window, not the whole log. Without
    /// these two fields a `jq '.comments | length'` reads as a total.
    #[test]
    fn issue_details_marks_truncated_comments() {
        let details = IssueDetails {
            issue: base_issue("bd-3", "Bounded"),
            comment_count: 12,
            comments_truncated: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"comment_count\":12"));
        assert!(json.contains("\"comments_truncated\":true"));
    }

    /// Conversely, an untruncated view must not carry a truthy truncation
    /// flag: "did I get everything?" has to be answerable, in both
    /// directions, from the payload alone.
    #[test]
    fn issue_details_omits_truncation_flag_when_complete() {
        let details = IssueDetails {
            issue: base_issue("bd-3", "Complete"),
            comment_count: 2,
            comments_truncated: false,
            ..Default::default()
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"comment_count\":2"));
        assert!(!json.contains("comments_truncated"));
    }

    #[test]
    fn blocked_issue_serializes_blockers() {
        let issue = base_issue("bd-3", "Blocked");
        let blocked = BlockedIssue {
            issue,
            blocked_by_count: 2,
            blocked_by: vec!["bd-a".to_string(), "bd-b".to_string()],
        };

        let json = serde_json::to_string(&blocked).unwrap();
        assert!(json.contains("\"blocked_by_count\":2"));
        assert!(json.contains("\"blocked_by\":[\"bd-a\",\"bd-b\"]"));
    }
}
