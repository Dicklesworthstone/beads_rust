//! Update command implementation.

use crate::cli::UpdateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{DependencyType, Issue, Status};
use crate::output::OutputContext;
use crate::storage::{IssueUpdate, SqliteStorage};
use crate::util::id::IdResolver;
use crate::util::time::parse_flexible_timestamp;
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
pub fn execute(args: &UpdateArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let _json = cli.json.unwrap_or(false);
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;

    // `--prefix` no longer feeds a default-prefix-prepend resolution step
    // (removed — see docs/PLAN_REMOVE_BD_ISSUE_PREFIX.md §3c); partial-ID
    // resolution is prefix-agnostic via substring match regardless. The
    // flag is accepted for backwards compatibility but has no effect here;
    // `--reprefix` (below) is the flag that actually moves an issue between
    // prefix namespaces.

    let actor = config::resolve_actor(&config_layer);
    let resolver = build_resolver(&config_layer, &storage_ctx.storage);
    let resolved_ids = resolve_target_ids(args, &beads_dir, &resolver, &storage_ctx.storage)?;

    // --reprefix: move issue to a different prefix namespace.
    if let Some(ref new_prefix) = args.reprefix {
        return execute_reprefix(
            args,
            &resolved_ids,
            new_prefix.trim(),
            &actor,
            &beads_dir,
            &mut storage_ctx,
            ctx,
        );
    }

    let claim_exclusive = config::claim_exclusive_from_layer(&config_layer);
    let update = build_update(args, &actor, claim_exclusive)?;
    let has_updates = !update.is_empty()
        || !args.add_label.is_empty()
        || !args.remove_label.is_empty()
        || !args.set_labels.is_empty()
        || args.parent.is_some();

    let mut updated_issues: Vec<UpdatedIssueOutput> = Vec::new();

    let storage = &mut storage_ctx.storage;

    for id in &resolved_ids {
        // Get issue before update for change tracking
        let issue_before = storage.get_issue(id)?;

        // Claim guard is now inside the IMMEDIATE transaction (see IssueUpdate.expect_unassigned)
        // to prevent TOCTOU races between concurrent agents.

        // Check if transitioning to in_progress (via --claim or --status in_progress)
        // and if so, validate that the issue is not blocked
        let transitioning_to_in_progress = args.claim
            || args
                .status
                .as_ref()
                .is_some_and(|s| s.eq_ignore_ascii_case("in_progress"));

        if transitioning_to_in_progress && !args.force && storage.is_blocked(id)? {
            let blockers = storage.get_blockers(id)?;
            let blocker_list = if blockers.is_empty() {
                "blocking dependencies".to_string()
            } else {
                blockers.join(", ")
            };
            return Err(BeadsError::validation(
                "claim",
                format!("cannot claim blocked issue: {blocker_list}"),
            ));
        }

        // Warn if the target status matches the current status (redundant transition)
        if let (Some(issue_before), Some(target_status)) =
            (&issue_before, &update.status)
        {
            if issue_before.status == *target_status {
                warn_redundant_status(id, target_status, storage);
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
        if !args.set_labels.is_empty() {
            // Remove all then add new
            storage.remove_all_labels(id, &actor)?;
            // Join all flag values, then split by comma (handles both --set-labels a,b and --set-labels a --set-labels b)
            let combined = args.set_labels.join(",");
            for label in combined.split(',') {
                let label = label.trim();
                if !label.is_empty() {
                    LabelValidator::validate(label)
                        .map_err(|e| BeadsError::validation("label", e.message))?;
                    storage.add_label(id, label, &actor)?;
                }
            }
        }

        // Apply parent
        apply_parent_update(storage, id, args.parent.as_deref(), &resolver, &actor)?;

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        // Get issue after update for output
        let issue_after = storage.get_issue(id)?;

        if let Some(issue) = issue_after {
            if ctx.is_json() {
                updated_issues.push(UpdatedIssueOutput::from(&issue));
            } else if has_updates {
                print_update_summary(id, &issue.title, issue_before.as_ref(), &issue);
            } else {
                println!("No updates specified for {id}");
            }
        }
    }

    if ctx.is_json() {
        ctx.json_pretty(&updated_issues);
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

/// JSON output for a reprefixed issue.
#[derive(Serialize)]
struct ReprefixOutput {
    old_id: String,
    new_id: String,
    title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<ReprefixChildOutput>,
}

/// JSON output for a reprefixed child.
#[derive(Serialize)]
struct ReprefixChildOutput {
    old_id: String,
    new_id: String,
}

/// Execute the --reprefix sub-flow: move an issue to a new prefix.
fn execute_reprefix(
    args: &UpdateArgs,
    resolved_ids: &[String],
    new_prefix: &str,
    actor: &str,
    beads_dir: &std::path::Path,
    storage_ctx: &mut config::OpenStorageResult,
    ctx: &OutputContext,
) -> Result<()> {
    // Guard: exactly one id.
    if resolved_ids.len() != 1 {
        return Err(BeadsError::validation(
            "reprefix",
            "--reprefix requires exactly one issue id",
        ));
    }

    // Guard: incompatible with --claim.
    if args.claim {
        return Err(BeadsError::validation(
            "reprefix",
            "--reprefix cannot be combined with --claim",
        ));
    }

    // Guard: reject reserved operator prefix.
    config::assert_writable_prefix(new_prefix)?;

    let old_id = &resolved_ids[0];

    // Guard: reject if old_id is a child (contains \".\").
    if old_id.contains('.') {
        return Err(BeadsError::validation(
            "reprefix",
            "cannot reprefix a child issue directly; reprefix the root parent instead",
        ));
    }

    // Guard: reject same prefix.
    if let Some((current_prefix, _)) = crate::util::id::split_prefix_remainder(old_id) {
        if current_prefix == new_prefix {
            return Err(BeadsError::validation(
                "reprefix",
                format!("issue already has prefix '{new_prefix}'"),
            ));
        }
    }

    let storage = &mut storage_ctx.storage;
    let (new_id, child_renames) = storage.reprefix_issue(old_id, new_prefix, actor)?;

    // Update last-touched to new id.
    crate::util::set_last_touched_id(beads_dir, &new_id);

    if ctx.is_json() {
        let issue = storage.get_issue(&new_id)?;
        let title = issue.map_or_else(String::new, |i| i.title);
        let output = ReprefixOutput {
            old_id: old_id.clone(),
            title,
            children: child_renames
                .iter()
                .map(|(old, new)| ReprefixChildOutput {
                    old_id: old.clone(),
                    new_id: new.clone(),
                })
                .collect(),
            new_id,
        };
        ctx.json_pretty(&output);
    } else {
        println!("Reprefixed {old_id} \u{2192} {new_id}");
        for (old_child, new_child) in &child_renames {
            println!("  child: {old_child} \u{2192} {new_child}");
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
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

fn build_resolver(_config_layer: &config::ConfigLayer, _storage: &SqliteStorage) -> IdResolver {
    IdResolver::with_defaults()
}

fn resolve_target_ids(
    args: &UpdateArgs,
    beads_dir: &std::path::Path,
    resolver: &IdResolver,
    storage: &SqliteStorage,
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
        |id| storage.id_exists(id).unwrap_or(false),
        |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
    )?;

    Ok(resolved_ids.into_iter().map(|r| r.id).collect())
}

fn build_update(args: &UpdateArgs, actor: &str, claim_exclusive: bool) -> Result<IssueUpdate> {
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

    // Build update struct
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
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        skip_cache_rebuild: false,
        expect_unassigned: args.claim,
        claim_exclusive: args.claim && claim_exclusive,
        claim_actor: if args.claim {
            Some(actor.to_string())
        } else {
            None
        },
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

fn resolve_issue_id(resolver: &IdResolver, storage: &SqliteStorage, input: &str) -> Result<String> {
    resolver
        .resolve(
            input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )
        .map(|resolved| resolved.id)
}

fn apply_parent_update(
    storage: &mut SqliteStorage,
    issue_id: &str,
    parent: Option<&str>,
    resolver: &IdResolver,
    actor: &str,
) -> Result<()> {
    let Some(parent_value) = parent else {
        return Ok(());
    };

    if parent_value.is_empty() {
        storage.remove_parent(issue_id, actor)?;
        return Ok(());
    }

    // Use immutable reference to storage for resolution
    let parent_id = resolve_issue_id(resolver, storage, parent_value)?;
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
    parse_flexible_timestamp(s, "date")
}

/// Emit an stderr warning when an issue is being transitioned to a status it already has.
///
/// This warns AI agents (and humans) that another process may have already claimed or
/// modified the issue. The warning is non-blocking — the operation still proceeds.
pub(crate) fn warn_redundant_status(
    id: &str,
    current_status: &Status,
    storage: &crate::storage::SqliteStorage,
) {
    use crate::model::EventType;
    use crate::util::time::format_duration_ago;

    let status_str = current_status.as_str();
    // Find the most recent status_changed event to report who and when
    if let Ok(events) = storage.get_events(id, 20) {
        if let Some(evt) = events
            .iter()
            .find(|e| e.event_type == EventType::StatusChanged)
        {
            let ago = format_duration_ago(evt.created_at, Utc::now());
            let actor_info = if evt.actor.is_empty() {
                String::new()
            } else {
                format!(" by '{}'", evt.actor)
            };
            eprintln!(
                "warning: {id} is already '{status_str}' (set {ago}{actor_info}) — \
                 another agent may be working on this issue; consider re-checking before proceeding"
            );
            return;
        }
    }
    // Fallback if no event found
    eprintln!(
        "warning: {id} is already '{status_str}' — \
         another agent may be working on this issue; consider re-checking before proceeding"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::init_test_logging;
    use crate::model::Priority;
    use chrono::{Datelike, Timelike};
    use tracing::info;

    #[test]
    fn test_optional_string_field_with_value() {
        init_test_logging();
        info!("test_optional_string_field_with_value: starting");
        let result = optional_string_field(Some("test"));
        assert_eq!(result, Some(Some("test".to_string())));
        info!("test_optional_string_field_with_value: assertions passed");
    }

    #[test]
    fn test_optional_string_field_with_empty() {
        init_test_logging();
        info!("test_optional_string_field_with_empty: starting");
        let result = optional_string_field(Some(""));
        assert_eq!(result, Some(None));
        info!("test_optional_string_field_with_empty: assertions passed");
    }

    #[test]
    fn test_optional_string_field_with_none() {
        init_test_logging();
        info!("test_optional_string_field_with_none: starting");
        let result = optional_string_field(None);
        assert_eq!(result, None);
        info!("test_optional_string_field_with_none: assertions passed");
    }

    #[test]
    fn test_optional_date_field_with_valid() {
        init_test_logging();
        info!("test_optional_date_field_with_valid: starting");
        let result = optional_date_field(Some("2024-01-15T12:00:00Z")).unwrap();
        assert!(result.is_some());
        let date = result.unwrap().unwrap();
        assert_eq!(date.year(), 2024);
        assert_eq!(date.month(), 1);
        assert_eq!(date.day(), 15);
        info!("test_optional_date_field_with_valid: assertions passed");
    }

    #[test]
    fn test_optional_date_field_with_empty() {
        init_test_logging();
        info!("test_optional_date_field_with_empty: starting");
        let result = optional_date_field(Some("")).unwrap();
        assert_eq!(result, Some(None));
        info!("test_optional_date_field_with_empty: assertions passed");
    }

    #[test]
    fn test_optional_date_field_with_none() {
        init_test_logging();
        info!("test_optional_date_field_with_none: starting");
        let result = optional_date_field(None).unwrap();
        assert_eq!(result, None);
        info!("test_optional_date_field_with_none: assertions passed");
    }

    #[test]
    fn test_optional_date_field_invalid_format() {
        init_test_logging();
        info!("test_optional_date_field_invalid_format: starting");
        let result = optional_date_field(Some("not-a-date"));
        assert!(result.is_err());
        info!("test_optional_date_field_invalid_format: assertions passed");
    }

    #[test]
    fn test_parse_date_valid_rfc3339() {
        init_test_logging();
        info!("test_parse_date_valid_rfc3339: starting");
        let result = parse_date("2024-06-15T10:30:00+00:00").unwrap();
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 15);
        info!("test_parse_date_valid_rfc3339: assertions passed");
    }

    #[test]
    fn test_parse_date_with_timezone() {
        init_test_logging();
        info!("test_parse_date_with_timezone: starting");
        let result = parse_date("2024-12-25T08:00:00-05:00").unwrap();
        // Should be converted to UTC
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 12);
        assert_eq!(result.day(), 25);
        assert_eq!(result.hour(), 13); // 8:00 EST = 13:00 UTC
        info!("test_parse_date_with_timezone: assertions passed");
    }

    #[test]
    fn test_parse_date_invalid() {
        init_test_logging();
        info!("test_parse_date_invalid: starting");
        let result = parse_date("invalid");
        assert!(result.is_err());
        info!("test_parse_date_invalid: assertions passed");
    }

    #[test]
    fn test_parse_date_partial_date() {
        init_test_logging();
        info!("test_parse_date_partial_date: starting");
        // Partial dates without time should now succeed
        let result = parse_date("2024-01-15");
        assert!(result.is_ok());
        let date = result.unwrap();
        assert_eq!(date.year(), 2024);
        assert_eq!(date.month(), 1);
        assert_eq!(date.day(), 15);
        info!("test_parse_date_partial_date: assertions passed");
    }

    #[test]
    fn test_build_update_with_claim() {
        init_test_logging();
        info!("test_build_update_with_claim: starting");
        let args = UpdateArgs {
            claim: true,
            ..Default::default()
        };
        let update = build_update(&args, "test_actor", false).unwrap();
        assert_eq!(update.status, Some(Status::InProgress));
        assert_eq!(update.assignee, Some(Some("test_actor".to_string())));
        info!("test_build_update_with_claim: assertions passed");
    }

    #[test]
    fn test_build_update_with_status() {
        init_test_logging();
        info!("test_build_update_with_status: starting");
        let args = UpdateArgs {
            status: Some("closed".to_string()),
            ..Default::default()
        };
        let update = build_update(&args, "test_actor", false).unwrap();
        assert_eq!(update.status, Some(Status::Closed));
        // closed_at should be set
        assert!(update.closed_at.is_some());
        info!("test_build_update_with_status: assertions passed");
    }

    #[test]
    fn test_build_update_with_priority() {
        init_test_logging();
        info!("test_build_update_with_priority: starting");
        let args = UpdateArgs {
            priority: Some("1".to_string()),
            ..Default::default()
        };
        let update = build_update(&args, "test_actor", false).unwrap();
        assert_eq!(update.priority, Some(Priority(1)));
        info!("test_build_update_with_priority: assertions passed");
    }

    #[test]
    fn test_build_update_empty() {
        init_test_logging();
        info!("test_build_update_empty: starting");
        let args = UpdateArgs::default();
        let update = build_update(&args, "test_actor", false).unwrap();
        assert!(update.is_empty());
        info!("test_build_update_empty: assertions passed");
    }
}
