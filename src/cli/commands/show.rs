//! Show command implementation.

use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{format_priority_badge, format_status_label};
use crate::util::id::{IdResolver, ResolverConfig};
use std::fmt::Write as FmtWrite;

/// Execute the show command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or issues are not found.
pub fn execute(ids: Vec<String>, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

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

    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let use_color = config::should_use_color(&config_layer);

    let mut details_list = Vec::new();
    for id_input in target_ids {
        let resolution = resolver.resolve(
            &id_input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )?;

        // Fetch full details including comments and events
        if let Some(details) = storage.get_issue_details(&resolution.id, true, false, 10)? {
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
            print_issue_details(&details, use_color);
            println!("----------------------------------------");
        }
    }

    Ok(())
}

fn print_issue_details(details: &crate::format::IssueDetails, use_color: bool) {
    let output = format_issue_details(details, use_color);
    print!("{output}");
}

fn format_issue_details(details: &crate::format::IssueDetails, use_color: bool) -> String {
    let mut output = String::new();
    let issue = &details.issue;
    let priority_badge = format_priority_badge(&issue.priority, use_color);
    let status_label = format_status_label(&issue.status, use_color);
    let _ = writeln!(
        output,
        "{} {} {priority_badge} [{}]",
        issue.id, issue.title, status_label
    );

    if let Some(assignee) = &issue.assignee {
        let _ = writeln!(output, "Assignee: {assignee}");
    }

    if !details.labels.is_empty() {
        let _ = writeln!(output, "Labels: {}", details.labels.join(", "));
    }

    if let Some(desc) = &issue.description {
        output.push('\n');
        let _ = writeln!(output, "{desc}");
    }

    if !details.dependencies.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependencies:");
        for dep in &details.dependencies {
            let _ = writeln!(output, "  -> {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.dependents.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependents:");
        for dep in &details.dependents {
            let _ = writeln!(output, "  <- {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.comments.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Comments:");
        for comment in &details.comments {
            let _ = writeln!(
                output,
                "  [{}] {}: {}",
                comment.created_at.format("%Y-%m-%d %H:%M UTC"),
                comment.author,
                comment.body
            );
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::format_issue_details;
    use crate::format::{IssueDetails, IssueWithDependencyMetadata};
    use crate::model::{Comment, Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use crate::util::id::{IdResolver, ResolverConfig};
    use chrono::{TimeZone, Utc};
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

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
        init_logging();
        info!("test_show_retrieves_issue_by_id: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-001", "Test Issue");
        storage.create_issue(&issue, "tester").unwrap();

        let retrieved = storage.get_issue("bd-001").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "bd-001");
        assert_eq!(retrieved.title, "Test Issue");
        info!("test_show_retrieves_issue_by_id: assertions passed");
    }

    #[test]
    fn test_show_returns_none_for_missing_id() {
        init_logging();
        info!("test_show_returns_none_for_missing_id: starting");
        let storage = SqliteStorage::open_memory().unwrap();

        let retrieved = storage.get_issue("nonexistent").unwrap();
        assert!(retrieved.is_none());
        info!("test_show_returns_none_for_missing_id: assertions passed");
    }

    #[test]
    fn test_show_multiple_issues() {
        init_logging();
        info!("test_show_multiple_issues: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "First Issue");
        let issue2 = make_test_issue("bd-002", "Second Issue");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        let retrieved1 = storage.get_issue("bd-001").unwrap().unwrap();
        let retrieved2 = storage.get_issue("bd-002").unwrap().unwrap();

        assert_eq!(retrieved1.title, "First Issue");
        assert_eq!(retrieved2.title, "Second Issue");
        info!("test_show_multiple_issues: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization() {
        init_logging();
        info!("test_issue_json_serialization: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string_pretty(&issue).unwrap();

        assert!(json.contains("\"id\": \"bd-001\""));
        assert!(json.contains("\"title\": \"Test Issue\""));
        assert!(json.contains("\"status\": \"open\""));
        info!("test_issue_json_serialization: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization_multiple() {
        init_logging();
        info!("test_issue_json_serialization_multiple: starting");
        let issues = vec![
            make_test_issue("bd-001", "First"),
            make_test_issue("bd-002", "Second"),
        ];

        let json = serde_json::to_string_pretty(&issues).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "bd-001");
        assert_eq!(parsed[1]["id"], "bd-002");
        info!("test_issue_json_serialization_multiple: assertions passed");
    }

    #[test]
    fn test_show_resolves_full_id() {
        init_logging();
        info!("test_show_resolves_full_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve("bd-abc123", |id| id == "bd-abc123", |_hash| Vec::new())
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_full_id: assertions passed");
    }

    #[test]
    fn test_show_resolves_prefixed_id() {
        init_logging();
        info!("test_show_resolves_prefixed_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve("abc123", |id| id == "bd-abc123", |_hash| Vec::new())
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_prefixed_id: assertions passed");
    }

    #[test]
    fn test_show_resolves_partial_id() {
        init_logging();
        info!("test_show_resolves_partial_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve(
                "abc",
                |_id| false,
                |hash| {
                    if hash == "abc" {
                        vec!["bd-abc123".to_string()]
                    } else {
                        Vec::new()
                    }
                },
            )
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_partial_id: assertions passed");
    }

    #[test]
    fn test_show_not_found_error() {
        init_logging();
        info!("test_show_not_found_error: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let result = resolver.resolve("missing", |_id| false, |_hash| Vec::new());
        assert!(result.is_err());
        info!("test_show_not_found_error: assertions passed");
    }

    #[test]
    fn test_show_json_output_shape() {
        init_logging();
        info!("test_show_json_output_shape: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let details = IssueDetails {
            issue: issue.clone(),
            labels: vec!["bug".to_string()],
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
        };
        let json = serde_json::to_string_pretty(&vec![details]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], issue.id);
        assert!(parsed[0]["labels"].is_array());
        assert!(parsed[0]["dependencies"].is_array());
        info!("test_show_json_output_shape: assertions passed");
    }

    #[test]
    fn test_show_text_includes_dependencies_and_comments() {
        init_logging();
        info!("test_show_text_includes_dependencies_and_comments: starting");
        let mut issue = make_test_issue("bd-001", "Test Issue");
        issue.description = None;
        let details = IssueDetails {
            issue,
            labels: Vec::new(),
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: vec![Comment {
                id: 1,
                issue_id: "bd-001".to_string(),
                author: "alice".to_string(),
                body: "Looks good".to_string(),
                created_at: Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 0).unwrap(),
            }],
            events: Vec::new(),
            parent: None,
        };
        let output = format_issue_details(&details, false);
        assert!(output.contains("Dependencies:"));
        assert!(output.contains("-> bd-002 (blocks) - Dep"));
        assert!(output.contains("Comments:"));
        assert!(output.contains("alice: Looks good"));
        info!("test_show_text_includes_dependencies_and_comments: assertions passed");
    }
}
