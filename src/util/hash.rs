//! Content hashing for issue deduplication and sync.
//!
//! Uses SHA256 over stable ordered fields with null separators.
//! Matches classic bd behavior for export/import compatibility.

use sha2::{Digest, Sha256};

use crate::model::{Issue, IssueType, Priority, Status};

/// Trait for types that can produce a deterministic content hash.
pub trait ContentHashable {
    /// Compute the content hash for this value.
    fn content_hash(&self) -> String;
}

impl ContentHashable for Issue {
    fn content_hash(&self) -> String {
        content_hash(self)
    }
}

/// Compute SHA256 content hash for an issue.
///
/// Fields included (stable order with null separators):
/// - title, description, design, `acceptance_criteria`, notes
/// - status, priority, `issue_type`
/// - assignee, owner, `created_by`
/// - `external_ref`, `source_system`
/// - pinned, `is_template`
///
/// Fields excluded:
/// - id, `content_hash` (circular)
/// - labels, dependencies, comments, events (separate entities)
/// - timestamps (`created_at`, `updated_at`, `closed_at`, etc.)
/// - tombstone fields (`deleted_at`, `deleted_by`, `delete_reason`)
/// - `estimated_minutes`, `due_at`, `defer_until`
/// - `close_reason`, `closed_by_session`
/// - `deleted_at`, `deleted_by`, `delete_reason`
#[must_use]
pub fn content_hash(issue: &Issue) -> String {
    content_hash_from_parts(
        &issue.title,
        issue.description.as_deref(),
        issue.design.as_deref(),
        issue.acceptance_criteria.as_deref(),
        issue.notes.as_deref(),
        &issue.status,
        &issue.priority,
        &issue.issue_type,
        issue.assignee.as_deref(),
        issue.owner.as_deref(),
        issue.created_by.as_deref(),
        issue.external_ref.as_deref(),
        issue.source_system.as_deref(),
        issue.pinned,
        issue.is_template,
    )
}

/// Create a content hash from raw components (for import/validation).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn content_hash_from_parts(
    title: &str,
    description: Option<&str>,
    design: Option<&str>,
    acceptance_criteria: Option<&str>,
    notes: Option<&str>,
    status: &Status,
    priority: &Priority,
    issue_type: &IssueType,
    assignee: Option<&str>,
    owner: Option<&str>,
    created_by: Option<&str>,
    external_ref: Option<&str>,
    source_system: Option<&str>,
    pinned: bool,
    is_template: bool,
) -> String {
    let mut writer = HashFieldWriter::new();

    writer.field(title);
    writer.field_opt(description);
    writer.field_opt(design);
    writer.field_opt(acceptance_criteria);
    writer.field_opt(notes);
    writer.field(status.as_str());
    writer.field(&format!("P{}", priority.0));
    writer.field(issue_type.as_str());
    writer.field_opt(assignee);
    writer.field_opt(owner);
    writer.field_opt(created_by);
    writer.field_opt(external_ref);
    writer.field_opt(source_system);
    writer.field_bool(pinned);
    writer.field_bool(is_template);

    writer.finalize()
}

struct HashFieldWriter {
    hasher: Sha256,
}

impl HashFieldWriter {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn field(&mut self, value: &str) {
        if value.contains('\0') {
            self.hasher.update(value.replace('\0', " ").as_bytes());
        } else {
            self.hasher.update(value.as_bytes());
        }
        self.hasher.update(b"\x00");
    }

    fn field_opt(&mut self, value: Option<&str>) {
        self.field(value.unwrap_or(""));
    }

    fn field_bool(&mut self, value: bool) {
        self.field(if value { "true" } else { "false" });
    }

    fn finalize(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_issue() -> Issue {
        Issue {
            id: "bd-test123".to_string(),
            content_hash: None,
            title: "Test Issue".to_string(),
            description: Some("A test description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: chrono::Utc::now(),
            created_by: None,
            updated_at: chrono::Utc::now(),
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
    fn test_content_hash_deterministic() {
        let issue = make_test_issue();
        let hash1 = content_hash(&issue);
        let hash2 = content_hash(&issue);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_is_hex() {
        let issue = make_test_issue();
        let hash = content_hash(&issue);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_content_hash_changes_with_title() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.title = "Different Title".to_string();
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_ignores_timestamps() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.updated_at = chrono::Utc::now();
        let hash2 = content_hash(&issue);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_pinned() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.pinned = true;
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_created_by() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.created_by = Some("tester@example.com".to_string());
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_includes_source_system() {
        let mut issue = make_test_issue();
        let hash1 = content_hash(&issue);

        issue.source_system = Some("imported".to_string());
        let hash2 = content_hash(&issue);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_from_parts() {
        let issue = make_test_issue();
        let direct = content_hash(&issue);
        let from_parts = content_hash_from_parts(
            &issue.title,
            issue.description.as_deref(),
            issue.design.as_deref(),
            issue.acceptance_criteria.as_deref(),
            issue.notes.as_deref(),
            &issue.status,
            &issue.priority,
            &issue.issue_type,
            issue.assignee.as_deref(),
            issue.owner.as_deref(),
            issue.created_by.as_deref(),
            issue.external_ref.as_deref(),
            issue.source_system.as_deref(),
            issue.pinned,
            issue.is_template,
        );
        assert_eq!(direct, from_parts);
    }
}
