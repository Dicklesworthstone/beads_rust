//! Update command implementation.

use crate::cli::UpdateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{DependencyType, Issue, Status};
use crate::storage::{IssueUpdate, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use crate::validation::LabelValidator;
use chrono::{DateTime, Utc};
use serde::Serialize;

/// JSON output structure for updated issues.
#[derive(Serialize)]
struct UpdatedIssueOutput {
    id: String,
    title: String,
    status: String,
    priority: i32,
    updated_at: DateTime<Utc>,
}

impl From<&Issue> for UpdatedIssueOutput {
    fn from(issue: &Issue) -> Self {
        Self {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status.as_str().to_string(),
            priority: issue.priority.0,
            updated_at: issue.updated_at,
        }
    }
}

/// Execute the update command.
///
/// # Errors
///
/// Returns an error if database operations fail or validation errors occur.
pub fn execute(args: &UpdateArgs, cli: &config::CliOverrides) -> Result<()> {
    let json = cli.json.unwrap_or(false);
    let beads_dir = config::discover_beads_dir(None)?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let (resolver, all_ids) = build_resolver(&config_layer, &storage)?;
    let resolved_ids = resolve_target_ids(args, &beads_dir, &resolver, &all_ids)?;

    let update = build_update(args, &actor)?;
    let has_updates = !update.is_empty()
        || !args.add_label.is_empty()
        || !args.remove_label.is_empty()
        || args.set_labels.is_some()
        || args.parent.is_some();

    let mut updated_issues: Vec<UpdatedIssueOutput> = Vec::new();

    for id in &resolved_ids {
        // Get issue before update for change tracking
        let issue_before = storage.get_issue(id)?;

        if args.claim {
            if let Some(ref issue) = issue_before {
                if let Some(ref current_assignee) = issue.assignee {
                    if current_assignee != &actor {
                        return Err(BeadsError::validation(
                            "claim",
                            format!("issue already assigned to {current_assignee}"),
                        ));
                    }
                }
            }
        }

        // Apply basic field updates
        if !update.is_empty() {
            storage.update_issue(id, &update, &actor)?;
        }

        // Apply labels
        for label in &args.add_label {
            LabelValidator::validate(label)
                .map_err(|e| BeadsError::validation("label", e.message))?;
            storage.add_label(id, label, &actor)?;
        }
        for label in &args.remove_label {
            storage.remove_label(id, label, &actor)?;
        }
        if let Some(ref labels_str) = args.set_labels {
            // Remove all then add new
            storage.remove_all_labels(id, &actor)?;
            for label in labels_str.split(',') {
                let label = label.trim();
                if !label.is_empty() {
                    LabelValidator::validate(label)
                        .map_err(|e| BeadsError::validation("label", e.message))?;
                    storage.add_label(id, label, &actor)?;
                }
            }
        }

        // Apply parent
        apply_parent_update(
            &mut storage,
            id,
            args.parent.as_deref(),
            &resolver,
            &all_ids,
            &actor,
        )?;

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        // Get issue after update for output
        let issue_after = storage.get_issue(id)?;

        if let Some(issue) = issue_after {
            if json {
                updated_issues.push(UpdatedIssueOutput::from(&issue));
            } else if has_updates {
                print_update_summary(id, &issue.title, issue_before.as_ref(), &issue);
            } else {
                println!("No updates specified for {id}");
            }
        }
    }

    if json {
        let json_output = serde_json::to_string_pretty(&updated_issues)?;
        println!("{json_output}");
    }

    Ok(())
}

/// Print a summary of what changed for the issue.
fn print_update_summary(id: &str, title: &str, before: Option<&Issue>, after: &Issue) {
    println!("Updated {id}: {title}");

    if let Some(before) = before {
        // Status change
        if before.status != after.status {
            println!(
                "  status: {} → {}",
                before.status.as_str(),
                after.status.as_str()
            );
        }
        // Priority change
        if before.priority != after.priority {
            println!("  priority: P{} → P{}", before.priority.0, after.priority.0);
        }
        // Type change
        if before.issue_type != after.issue_type {
            println!(
                "  type: {} → {}",
                before.issue_type.as_str(),
                after.issue_type.as_str()
            );
        }
        // Assignee change
        if before.assignee != after.assignee {
            let before_assignee = before.assignee.as_deref().unwrap_or("(none)");
            let after_assignee = after.assignee.as_deref().unwrap_or("(none)");
            println!("  assignee: {before_assignee} → {after_assignee}");
        }
        // Owner change
        if before.owner != after.owner {
            let before_owner = before.owner.as_deref().unwrap_or("(none)");
            let after_owner = after.owner.as_deref().unwrap_or("(none)");
            println!("  owner: {before_owner} → {after_owner}");
        }
    }
}

fn build_resolver(
    config_layer: &config::ConfigLayer,
    storage: &SqliteStorage,
) -> Result<(IdResolver, Vec<String>)> {
    let id_config = config::id_config_from_layer(config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;
    Ok((resolver, all_ids))
}

fn resolve_target_ids(
    args: &UpdateArgs,
    beads_dir: &std::path::Path,
    resolver: &IdResolver,
    all_ids: &[String],
) -> Result<Vec<String>> {
    let mut ids = args.ids.clone();
    if ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        ids.push(last_touched);
    }

    let resolved_ids = resolver.resolve_all(
        &ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(all_ids, hash),
    )?;

    Ok(resolved_ids.into_iter().map(|r| r.id).collect())
}

fn build_update(args: &UpdateArgs, actor: &str) -> Result<IssueUpdate> {
    let status = if args.claim {
        Some(Status::InProgress)
    } else {
        args.status.as_ref().map(|s| s.parse()).transpose()?
    };

    let priority = args.priority.as_ref().map(|p| p.parse()).transpose()?;

    let issue_type = args.type_.as_ref().map(|t| t.parse()).transpose()?;

    let assignee = if args.claim {
        Some(Some(actor.to_string()))
    } else {
        optional_string_field(args.assignee.as_deref())
    };

    let owner = optional_string_field(args.owner.as_deref());
    let due_at = optional_date_field(args.due.as_deref())?;
    let defer_until = optional_date_field(args.defer.as_deref())?;

    let closed_at = match &status {
        Some(Status::Closed | Status::Tombstone) => Some(Some(Utc::now())),
        Some(Status::Open | Status::InProgress) => Some(None),
        _ => None,
    };

    Ok(IssueUpdate {
        title: args.title.clone(),
        description: args.description.clone().map(Some),
        design: args.design.clone().map(Some),
        acceptance_criteria: args.acceptance_criteria.clone().map(Some),
        notes: args.notes.clone().map(Some),
        status,
        priority,
        issue_type,
        assignee,
        owner,
        estimated_minutes: args.estimate.map(Some),
        due_at,
        defer_until,
        external_ref: optional_string_field(args.external_ref.as_deref()),
        closed_at,
        close_reason: None,
        closed_by_session: args.session.clone().map(Some),
    })
}

#[allow(clippy::option_option, clippy::single_option_map)]
fn optional_string_field(value: Option<&str>) -> Option<Option<String>> {
    value.map(|v| {
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    })
}

#[allow(clippy::option_option)]
fn optional_date_field(value: Option<&str>) -> Result<Option<Option<DateTime<Utc>>>> {
    value
        .map(|v| {
            if v.is_empty() {
                Ok(None)
            } else {
                parse_date(v).map(Some)
            }
        })
        .transpose()
}

fn resolve_issue_id(resolver: &IdResolver, all_ids: &[String], input: &str) -> Result<String> {
    resolver
        .resolve(
            input,
            |id| all_ids.iter().any(|existing| existing == id),
            |hash| find_matching_ids(all_ids, hash),
        )
        .map(|resolved| resolved.id)
}

fn apply_parent_update(
    storage: &mut SqliteStorage,
    issue_id: &str,
    parent: Option<&str>,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
) -> Result<()> {
    let Some(parent_value) = parent else {
        return Ok(());
    };

    if parent_value.is_empty() {
        storage.remove_parent(issue_id, actor)?;
        return Ok(());
    }

    let parent_id = resolve_issue_id(resolver, all_ids, parent_value)?;
    if parent_id == issue_id {
        return Err(BeadsError::validation(
            "parent",
            "issue cannot be its own parent",
        ));
    }

    storage.remove_parent(issue_id, actor)?;
    storage.add_dependency(
        issue_id,
        &parent_id,
        DependencyType::ParentChild.as_str(),
        actor,
    )?;
    Ok(())
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    // Basic RFC3339 parsing for now
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| BeadsError::validation("date", "invalid format (expected RFC3339)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Priority;
    use chrono::{Datelike, Timelike};

    #[test]
    fn test_optional_string_field_with_value() {
        let result = optional_string_field(Some("test"));
        assert_eq!(result, Some(Some("test".to_string())));
    }

    #[test]
    fn test_optional_string_field_with_empty() {
        let result = optional_string_field(Some(""));
        assert_eq!(result, Some(None));
    }

    #[test]
    fn test_optional_string_field_with_none() {
        let result = optional_string_field(None);
        assert_eq!(result, None);
    }

    #[test]
    fn test_optional_date_field_with_valid() {
        let result = optional_date_field(Some("2024-01-15T12:00:00Z")).unwrap();
        assert!(result.is_some());
        let date = result.unwrap().unwrap();
        assert_eq!(date.year(), 2024);
        assert_eq!(date.month(), 1);
        assert_eq!(date.day(), 15);
    }

    #[test]
    fn test_optional_date_field_with_empty() {
        let result = optional_date_field(Some("")).unwrap();
        assert_eq!(result, Some(None));
    }

    #[test]
    fn test_optional_date_field_with_none() {
        let result = optional_date_field(None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_optional_date_field_invalid_format() {
        let result = optional_date_field(Some("not-a-date"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_date_valid_rfc3339() {
        let result = parse_date("2024-06-15T10:30:00+00:00").unwrap();
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 15);
    }

    #[test]
    fn test_parse_date_with_timezone() {
        let result = parse_date("2024-12-25T08:00:00-05:00").unwrap();
        // Should be converted to UTC
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 12);
        assert_eq!(result.day(), 25);
        assert_eq!(result.hour(), 13); // 8:00 EST = 13:00 UTC
    }

    #[test]
    fn test_parse_date_invalid() {
        let result = parse_date("invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_date_partial_date() {
        // Partial dates without time should fail
        let result = parse_date("2024-01-15");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_update_with_claim() {
        let args = UpdateArgs {
            claim: true,
            ..Default::default()
        };
        let update = build_update(&args, "test_actor").unwrap();
        assert_eq!(update.status, Some(Status::InProgress));
        assert_eq!(update.assignee, Some(Some("test_actor".to_string())));
    }

    #[test]
    fn test_build_update_with_status() {
        let args = UpdateArgs {
            status: Some("closed".to_string()),
            ..Default::default()
        };
        let update = build_update(&args, "test_actor").unwrap();
        assert_eq!(update.status, Some(Status::Closed));
        // closed_at should be set
        assert!(update.closed_at.is_some());
    }

    #[test]
    fn test_build_update_with_priority() {
        let args = UpdateArgs {
            priority: Some("1".to_string()),
            ..Default::default()
        };
        let update = build_update(&args, "test_actor").unwrap();
        assert_eq!(update.priority, Some(Priority(1)));
    }

    #[test]
    fn test_build_update_empty() {
        let args = UpdateArgs::default();
        let update = build_update(&args, "test_actor").unwrap();
        assert!(update.is_empty());
    }
}
