//! Close command implementation.

use crate::cli::CloseArgs as CliCloseArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use chrono::Utc;
use serde::Serialize;

/// Internal arguments for the close command.
#[derive(Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close
    pub ids: Vec<String>,
    /// Close reason
    pub reason: Option<String>,
    /// Force close even if blocked
    pub force: bool,
    /// Session ID for `closed_by_session` field
    pub session: Option<String>,
    /// Return newly unblocked issues (single ID only)
    pub suggest_next: bool,
}

impl From<&CliCloseArgs> for CloseArgs {
    fn from(cli: &CliCloseArgs) -> Self {
        Self {
            ids: cli.ids.clone(),
            reason: cli.reason.clone(),
            force: cli.force,
            session: cli.session.clone(),
            suggest_next: cli.suggest_next,
        }
    }
}

/// Execute the close command from CLI args.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_cli(cli_args: &CliCloseArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let args = CloseArgs::from(cli_args);
    execute_with_args(&args, json, cli)
}

/// Result of a close operation for JSON output.
#[derive(Debug, Serialize)]
pub struct CloseResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
}

/// Result of closing with suggest-next.
#[derive(Debug, Serialize)]
pub struct CloseWithSuggestResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
    pub unblocked: Vec<UnblockedIssue>,
}

/// An issue that became unblocked after closing.
#[derive(Debug, Serialize)]
pub struct UnblockedIssue {
    pub id: String,
    pub title: String,
    pub priority: i32,
}

#[derive(Debug, Serialize)]
pub struct ClosedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub closed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

/// Execute the close command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute(ids: Vec<String>, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let args = CloseArgs {
        ids,
        reason: None,
        force: false,
        session: None,
        suggest_next: false,
    };

    execute_with_args(&args, json, cli)
}

/// Execute the close command with full arguments.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute_with_args(args: &CloseArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    tracing::info!("Executing close command");

    let beads_dir = config::discover_beads_dir(None)?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;

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

    // Validate suggest-next only works with single ID
    if args.suggest_next && ids.len() > 1 {
        return Err(BeadsError::validation(
            "suggest-next",
            "--suggest-next only works with a single issue ID",
        ));
    }

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    // Track blocked issues before closing (for suggest-next)
    let blocked_before: Vec<String> = if args.suggest_next {
        storage
            .get_blocked_issues()?
            .into_iter()
            .map(|(i, _)| i.id)
            .collect()
    } else {
        Vec::new()
    };

    let mut closed_issues: Vec<ClosedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, "Closing issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if already closed
        if issue.status.is_terminal() {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
            });
            continue;
        }

        // Check if blocked (unless --force)
        if !args.force && storage.is_blocked(id)? {
            let mut blocker_ids = storage
                .get_blocked_issues()?
                .into_iter()
                .find(|(issue, _)| issue.id == *id)
                .map(|(_, blockers)| blockers)
                .unwrap_or_default();
            if blocker_ids.is_empty() {
                blocker_ids = storage.get_dependencies(id)?;
            }
            tracing::debug!(blocked_by = ?blocker_ids, "Issue is blocked");
            let reason = if blocker_ids.is_empty() {
                "blocked by dependencies".to_string()
            } else {
                format!("blocked by: {}", blocker_ids.join(", "))
            };
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason,
            });
            continue;
        }

        // Build update
        let now = Utc::now();
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: args.reason.clone().map(Some),
            closed_by_session: args.session.clone().map(Some),
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, reason = ?args.reason, "Issue closed");

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        closed_issues.push(ClosedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "closed".to_string(),
            closed_at: now.to_rfc3339(),
            close_reason: args.reason.clone(),
        });
    }

    // Handle suggest-next: find issues that became unblocked
    let unblocked_issues: Vec<UnblockedIssue> = if args.suggest_next && !closed_issues.is_empty() {
        // Rebuild blocked cache to reflect the closure
        storage.rebuild_blocked_cache(true)?;

        // Find issues that were blocked before but aren't now
        let blocked_after: Vec<String> = storage
            .get_blocked_issues()?
            .into_iter()
            .map(|(i, _)| i.id)
            .collect();

        let newly_unblocked: Vec<String> = blocked_before
            .into_iter()
            .filter(|id| !blocked_after.contains(id))
            .collect();

        tracing::debug!(unblocked = ?newly_unblocked, "Issues unblocked by close");

        let mut unblocked = Vec::new();
        for uid in newly_unblocked {
            if let Some(issue) = storage.get_issue(&uid)? {
                unblocked.push(UnblockedIssue {
                    id: issue.id,
                    title: issue.title,
                    priority: issue.priority.0,
                });
            }
        }
        unblocked
    } else if !closed_issues.is_empty() {
        // Rebuild blocked cache even if not suggest-next
        tracing::info!(
            "Rebuilding blocked cache after closing {} issues",
            closed_issues.len()
        );
        storage.rebuild_blocked_cache(true)?;
        Vec::new()
    } else {
        Vec::new()
    };

    // Output
    if json {
        if args.suggest_next {
            let result = CloseWithSuggestResult {
                closed: closed_issues,
                skipped: skipped_issues,
                unblocked: unblocked_issues,
            };
            let output = serde_json::to_string_pretty(&result).map_err(BeadsError::Json)?;
            println!("{output}");
        } else {
            let result = CloseResult {
                closed: closed_issues,
                skipped: skipped_issues,
            };
            let output = serde_json::to_string_pretty(&result).map_err(BeadsError::Json)?;
            println!("{output}");
        }
    } else {
        for closed in &closed_issues {
            print!("\u{2713} Closed {}: {}", closed.id, closed.title);
            if let Some(reason) = &closed.close_reason {
                println!(" ({reason})");
            } else {
                println!();
            }
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if !unblocked_issues.is_empty() {
            let ids: Vec<&str> = unblocked_issues.iter().map(|i| i.id.as_str()).collect();
            println!("  Unblocked: {}", ids.join(", "));
        }
        if closed_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to close.");
        }
    }

    Ok(())
}
