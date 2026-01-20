#![allow(dead_code)]

use beads_rust::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use chrono::{Duration, TimeZone, Utc};

/// Base time for test fixtures - set in the past to allow tests to manipulate
/// `updated_at` without violating the `created_at` <= `updated_at` constraint.
fn base_time() -> chrono::DateTime<Utc> {
    // Use a fixed timestamp to ensure deterministic IDs for snapshot tests
    Utc.timestamp_opt(1_735_689_600, 0).unwrap() // 2025-01-01 00:00:00 UTC
}

pub fn issue(title: &str) -> Issue {
    let base = base_time();
    Issue {
        id: format!("test-{}", hash_title(title)),
        title: title.to_string(),
        description: None,
        issue_type: IssueType::Task,
        status: Status::Open,
        priority: Priority::MEDIUM,
        assignee: None,
        labels: vec![],
        created_at: base,
        updated_at: base + Duration::seconds(1),
        content_hash: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        owner: None,
        estimated_minutes: None,
        created_by: None,
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
        dependencies: vec![],
        comments: vec![],
    }
}

fn hash_title(title: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    format!("{:x}", hasher.finalize())[..8].to_string()
}

pub struct IssueBuilder {
    issue: Issue,
}

impl IssueBuilder {
    pub fn new(title: &str) -> Self {
        Self {
            issue: issue(title),
        }
    }

    pub fn with_type(mut self, t: IssueType) -> Self {
        self.issue.issue_type = t;
        self
    }

    pub fn with_status(mut self, s: Status) -> Self {
        self.issue.status = s;
        self
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn with_priority(mut self, p: Priority) -> Self {
        self.issue.priority = p;
        self
    }

    pub fn with_assignee(mut self, assignee: &str) -> Self {
        self.issue.assignee = Some(assignee.to_string());
        self
    }

    pub fn with_description(mut self, description: &str) -> Self {
        self.issue.description = Some(description.to_string());
        self
    }

    pub fn with_id(mut self, id: &str) -> Self {
        self.issue.id = id.to_string();
        self
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn with_template(mut self) -> Self {
        self.issue.is_template = true;
        self
    }

    pub fn build(self) -> Issue {
        self.issue
    }
}

pub fn dependency(from: &str, to: &str) -> Dependency {
    Dependency {
        issue_id: from.to_string(),
        depends_on_id: to.to_string(),
        dep_type: DependencyType::Blocks,
        created_at: Utc::now(),
        created_by: None,
        metadata: None,
        thread_id: None,
    }
}
