use crate::model::{Comment, Event, Issue, IssueType, Priority, Status};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Minimal issue output for stale command (bd parity).
/// Contains only the fields that bd's stale command outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueWithCounts {
    #[serde(flatten)]
    pub issue: Issue,
    pub dependency_count: usize,
    pub dependent_count: usize,
}

/// Issue details with full relations for show view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDetails {
    #[serde(flatten)]
    pub issue: Issue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<IssueWithDependencyMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependents: Vec<IssueWithDependencyMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueWithDependencyMetadata {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub priority: Priority,
    pub dep_type: String,
}

/// Blocked issue for blocked view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedIssue {
    #[serde(flatten)]
    pub issue: Issue,
    pub blocked_by_count: usize,
    pub blocked_by: Vec<String>,
}

/// Tree node for dependency tree view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    #[serde(flatten)]
    pub issue: Issue,
    pub depth: usize,
    pub parent_id: Option<String>,
    pub truncated: bool,
}

/// Summary statistics for the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breakdown {
    pub dimension: String,
    pub counts: Vec<BreakdownEntry>,
}

/// A single entry in a breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakdownEntry {
    pub key: String,
    pub count: usize,
}

/// Recent activity statistics from git history.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        };

        let json = serde_json::to_string(&iwc).unwrap();
        assert!(json.contains("\"dependency_count\":2"));
        assert!(json.contains("\"dependent_count\":1"));
        assert!(json.contains("\"id\":\"bd-1\""));
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
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"parent\":\"bd-parent\""));
        assert!(json.contains("\"labels\":[\"backend\"]"));
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
