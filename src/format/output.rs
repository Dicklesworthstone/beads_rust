use crate::model::{Comment, Event, Issue, Priority, Status};
use serde::{Deserialize, Serialize};

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
    pub labels: Vec<String>,
    pub dependencies: Vec<IssueWithDependencyMetadata>,
    pub dependents: Vec<IssueWithDependencyMetadata>,
    pub comments: Vec<Comment>,
    pub events: Vec<Event>,
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

/// Aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statistics {
    // TODO: Define stats structure
    pub total: usize,
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
