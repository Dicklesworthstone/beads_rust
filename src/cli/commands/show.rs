//! Show command implementation.

use crate::config;
use crate::error::{BeadsError, Result};
use crate::util::id::{IdResolver, ResolverConfig};

/// Execute the show command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or issues are not found.
pub fn execute(ids: Vec<String>, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let mut target_ids = ids;
    if target_ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_ids.push(last_touched);
    }

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    let mut details_list = Vec::new();
    for id_input in target_ids {
        let resolution = resolver.resolve(
            &id_input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )?;

        // Fetch full details including comments and events
        if let Some(details) = storage.get_issue_details(&resolution.id, true, true, 10)? {
            details_list.push(details);
        } else {
            return Err(BeadsError::IssueNotFound { id: resolution.id });
        }
    }

    if json {
        // Output full details as JSON
        let output = serde_json::to_string_pretty(&details_list)?;
        println!("{output}");
    } else {
        for details in details_list {
            print_issue_details(&details);
            println!("----------------------------------------");
        }
    }

    Ok(())
}

fn print_issue_details(details: &crate::format::IssueDetails) {
    let issue = &details.issue;
    println!(
        "{} {} [{}] [{}]",
        issue.id, issue.title, issue.priority, issue.status
    );

    if let Some(assignee) = &issue.assignee {
        println!("Assignee: {assignee}");
    }

    if !details.labels.is_empty() {
        println!("Labels: {}", details.labels.join(", "));
    }

    if let Some(desc) = &issue.description {
        println!("\n{desc}");
    }

    if !details.dependencies.is_empty() {
        println!("\nDependencies:");
        for dep in &details.dependencies {
            println!("  -> {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.dependents.is_empty() {
        println!("\nDependents:");
        for dep in &details.dependents {
            println!("  <- {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.comments.is_empty() {
        println!("\nComments:");
        for comment in &details.comments {
            println!(
                "  [{}] {}: {}",
                comment.created_at.format("%Y-%m-%d %H:%M"),
                comment.author,
                comment.body
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::{TimeZone, Utc};

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: Some("Test description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
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
    fn test_show_retrieves_issue_by_id() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-001", "Test Issue");
        storage.create_issue(&issue, "tester").unwrap();

        let retrieved = storage.get_issue("bd-001").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "bd-001");
        assert_eq!(retrieved.title, "Test Issue");
    }

    #[test]
    fn test_show_returns_none_for_missing_id() {
        let storage = SqliteStorage::open_memory().unwrap();

        let retrieved = storage.get_issue("nonexistent").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_show_multiple_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "First Issue");
        let issue2 = make_test_issue("bd-002", "Second Issue");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        let retrieved1 = storage.get_issue("bd-001").unwrap().unwrap();
        let retrieved2 = storage.get_issue("bd-002").unwrap().unwrap();

        assert_eq!(retrieved1.title, "First Issue");
        assert_eq!(retrieved2.title, "Second Issue");
    }

    #[test]
    fn test_issue_json_serialization() {
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string_pretty(&issue).unwrap();

        assert!(json.contains("\"id\": \"bd-001\""));
        assert!(json.contains("\"title\": \"Test Issue\""));
        assert!(json.contains("\"status\": \"open\""));
    }

    #[test]
    fn test_issue_json_serialization_multiple() {
        let issues = vec![
            make_test_issue("bd-001", "First"),
            make_test_issue("bd-002", "Second"),
        ];

        let json = serde_json::to_string_pretty(&issues).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "bd-001");
        assert_eq!(parsed[1]["id"], "bd-002");
    }
}
