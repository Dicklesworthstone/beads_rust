//! Reopen command implementation.

use crate::cli::ReopenArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use serde::Serialize;

/// Result of reopening a single issue.
#[derive(Debug, Serialize)]
pub struct ReopenedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

/// Issue that was skipped during reopen.
#[derive(Debug, Serialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

/// JSON output for reopen command.
#[derive(Debug, Serialize)]
pub struct ReopenResult {
    pub reopened: Vec<ReopenedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
}

/// Execute the reopen command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &ReopenArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let use_json = json || args.robot;

    tracing::info!("Executing reopen command");

    let beads_dir = config::discover_beads_dir(None)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let storage = &mut storage_ctx.storage;

    // Get IDs - use last touched if none provided
    let mut ids = args.ids.clone();
    if ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        ids.push(last_touched);
    }

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    let mut reopened_issues: Vec<ReopenedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, "Reopening issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if already open
        if !issue.status.is_terminal() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue already open");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
            });
            continue;
        }

        tracing::debug!(previous_status = ?issue.status, "Issue was previously {:?}", issue.status);

        // Build update: set status=open, clear closed_at
        let update = IssueUpdate {
            status: Some(Status::Open),
            closed_at: Some(None),         // Clear closed_at
            close_reason: Some(None),      // Clear close_reason
            closed_by_session: Some(None), // Clear closed_by_session
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, reason = ?args.reason, "Issue reopened");

        // Add comment if reason provided
        if let Some(ref reason) = args.reason {
            let comment_text = format!("Reopened: {reason}");
            tracing::debug!(id = %id, "Adding reopen comment");
            storage.add_comment(id, &actor, &comment_text)?;
        }

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        reopened_issues.push(ReopenedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "open".to_string(),
            closed_at: None,
        });
    }

    // Output
    if use_json {
        let result = ReopenResult {
            reopened: reopened_issues,
            skipped: skipped_issues,
        };
        let output = serde_json::to_string_pretty(&result).map_err(BeadsError::Json)?;
        println!("{output}");
    } else {
        for reopened in &reopened_issues {
            print!("\u{2713} Reopened {}: {}", reopened.id, reopened.title);
            if let Some(ref reason) = args.reason {
                println!(" ({reason})");
            } else {
                println!();
            }
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if reopened_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to reopen.");
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}
