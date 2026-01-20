//! Close command implementation.

use crate::cli::CloseArgs as CliCloseArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::output::OutputContext;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use chrono::Utc;
use serde::{Deserialize, Serialize};

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
pub fn execute_cli(
    cli_args: &CliCloseArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let args = CloseArgs::from(cli_args);
    execute_with_args(&args, json, cli, ctx)
}

/// Result of a close operation for JSON output.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
}

/// Result of closing with suggest-next.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseWithSuggestResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    pub unblocked: Vec<UnblockedIssue>,
}

/// An issue that became unblocked after closing.
#[derive(Debug, Serialize, Deserialize)]
pub struct UnblockedIssue {
    pub id: String,
    pub title: String,
    pub priority: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClosedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub closed_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub close_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

/// Execute the close command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute(
    ids: Vec<String>,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let args = CloseArgs {
        ids,
        reason: None,
        force: false,
        session: None,
        suggest_next: false,
    };

    execute_with_args(&args, json, cli, ctx)
}

/// Execute the close command with full arguments.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute_with_args(
    args: &CloseArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    tracing::info!("Executing close command");

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
        let close_reason = args.reason.clone().unwrap_or_else(|| "done".to_string());
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: Some(Some(close_reason.clone())),
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
            close_reason: Some(close_reason),
        });
    }

    // Handle suggest-next: find issues that became unblocked
    let unblocked_issues: Vec<UnblockedIssue> = if args.suggest_next && !closed_issues.is_empty() {
        // Rebuild blocked cache to reflect the closure
        // Note: storage.update_issue already triggered a transactional cache rebuild if status changed.
        // We just need to fetch the new state.

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
    } else {
        Vec::new()
    };

    // Output
    if ctx.is_json() {
        if args.suggest_next {
            // suggest_next is br-only, use wrapped format
            let result = CloseWithSuggestResult {
                closed: closed_issues,
                skipped: skipped_issues,
                unblocked: unblocked_issues,
            };
            ctx.json_pretty(&result);
        } else {
            // bd conformance: output bare array of closed issues
            ctx.json_pretty(&closed_issues);
        }
    } else {
        if closed_issues.is_empty() && skipped_issues.is_empty() {
            ctx.info("No issues to close.");
        } else {
            for closed in &closed_issues {
                let mut msg = format!("Closed {}: {}", closed.id, closed.title);
                if let Some(reason) = &closed.close_reason {
                    msg.push_str(&format!(" ({reason})"));
                }
                ctx.success(&msg);
            }
            for skipped in &skipped_issues {
                ctx.warning(&format!("Skipped {}: {}", skipped.id, skipped.reason));
            }
            if !unblocked_issues.is_empty() {
                ctx.newline();
                ctx.info(&format!("Unblocked {} issue(s):", unblocked_issues.len()));
                for issue in &unblocked_issues {
                    ctx.print(&format!("  {}: {}", issue.id, issue.title));
                }
            }
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // CloseArgs tests
    // =========================================================================

    #[test]
    fn test_close_args_default() {
        let args = CloseArgs::default();
        assert!(args.ids.is_empty());
        assert!(args.reason.is_none());
        assert!(!args.force);
        assert!(args.session.is_none());
        assert!(!args.suggest_next);
    }

    #[test]
    fn test_close_args_with_all_fields() {
        let args = CloseArgs {
            ids: vec!["bd-abc".to_string(), "bd-xyz".to_string()],
            reason: Some("Fixed in PR #123".to_string()),
            force: true,
            session: Some("session-456".to_string()),
            suggest_next: true,
        };
        assert_eq!(args.ids.len(), 2);
        assert_eq!(args.ids[0], "bd-abc");
        assert_eq!(args.reason.as_deref(), Some("Fixed in PR #123"));
        assert!(args.force);
        assert_eq!(args.session.as_deref(), Some("session-456"));
        assert!(args.suggest_next);
    }

    // =========================================================================
    // CloseResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_result_serialization_empty_skipped_omitted() {
        let result = CloseResult {
            closed: vec![ClosedIssue {
                id: "bd-123".to_string(),
                title: "Test issue".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: None,
            }],
            skipped: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // Empty skipped should be omitted due to skip_serializing_if
        assert!(!json.contains("\"skipped\""));
        assert!(json.contains("\"closed\""));
    }

    #[test]
    fn test_close_result_serialization_with_skipped() {
        let result = CloseResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-456".to_string(),
                reason: "already closed".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"skipped\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_close_result_roundtrip() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-a".to_string(),
                    title: "First".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: Some("Done".to_string()),
                },
                ClosedIssue {
                    id: "bd-b".to_string(),
                    title: "Second".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-02T00:00:00Z".to_string(),
                    close_reason: None,
                },
            ],
            skipped: vec![SkippedIssue {
                id: "bd-c".to_string(),
                reason: "blocked by: bd-d".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 1);
        assert_eq!(parsed.closed[0].id, "bd-a");
        assert_eq!(parsed.closed[0].close_reason.as_deref(), Some("Done"));
        assert!(parsed.closed[1].close_reason.is_none());
    }

    // =========================================================================
    // CloseWithSuggestResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_with_suggest_result_serialization() {
        let result = CloseWithSuggestResult {
            closed: vec![ClosedIssue {
                id: "bd-parent".to_string(),
                title: "Parent task".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-15T10:00:00Z".to_string(),
                close_reason: Some("Completed".to_string()),
            }],
            skipped: vec![],
            unblocked: vec![
                UnblockedIssue {
                    id: "bd-child1".to_string(),
                    title: "Child task 1".to_string(),
                    priority: 1,
                },
                UnblockedIssue {
                    id: "bd-child2".to_string(),
                    title: "Child task 2".to_string(),
                    priority: 2,
                },
            ],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"unblocked\""));
        assert!(json.contains("\"bd-child1\""));
        assert!(json.contains("\"bd-child2\""));
        assert!(json.contains("\"priority\":1"));
        assert!(json.contains("\"priority\":2"));
        // Empty skipped should be omitted
        assert!(!json.contains("\"skipped\""));
    }

    #[test]
    fn test_close_with_suggest_result_empty_unblocked() {
        let result = CloseWithSuggestResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-x".to_string(),
                reason: "not found".to_string(),
            }],
            unblocked: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // unblocked is not marked skip_serializing_if, so it should appear as empty array
        assert!(json.contains("\"unblocked\":[]"));
        assert!(json.contains("\"skipped\""));
    }

    // =========================================================================
    // ClosedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_closed_issue_serialization_with_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: Some("Fixed in commit abc123".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"close_reason\":\"Fixed in commit abc123\""));
    }

    #[test]
    fn test_closed_issue_serialization_without_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: None,
        };
        let json = serde_json::to_string(&issue).unwrap();
        // close_reason should be omitted due to skip_serializing_if
        assert!(!json.contains("close_reason"));
    }

    #[test]
    fn test_closed_issue_all_fields() {
        let issue = ClosedIssue {
            id: "beads_rust-xyz".to_string(),
            title: "Multi-word title with special chars: <>&".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-12-31T23:59:59Z".to_string(),
            close_reason: Some("End of year cleanup".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let parsed: ClosedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "beads_rust-xyz");
        assert!(parsed.title.contains("<>&"));
        assert_eq!(parsed.status, "closed");
        assert!(parsed.closed_at.contains("2026-12-31"));
    }

    // =========================================================================
    // SkippedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_skipped_issue_serialization() {
        let skipped = SkippedIssue {
            id: "bd-skip".to_string(),
            reason: "already closed".to_string(),
        };
        let json = serde_json::to_string(&skipped).unwrap();
        assert!(json.contains("\"id\":\"bd-skip\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_skipped_issue_blocked_reason() {
        let skipped = SkippedIssue {
            id: "bd-blocked".to_string(),
            reason: "blocked by: bd-dep1, bd-dep2".to_string(),
        };
        let json = serde_json::to_string(&skipped).unwrap();
        let parsed: SkippedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "bd-blocked");
        assert!(parsed.reason.contains("bd-dep1"));
        assert!(parsed.reason.contains("bd-dep2"));
    }

    // =========================================================================
    // UnblockedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_unblocked_issue_serialization() {
        let unblocked = UnblockedIssue {
            id: "bd-next".to_string(),
            title: "Next task".to_string(),
            priority: 1,
        };
        let json = serde_json::to_string(&unblocked).unwrap();
        assert!(json.contains("\"id\":\"bd-next\""));
        assert!(json.contains("\"title\":\"Next task\""));
        assert!(json.contains("\"priority\":1"));
    }

    #[test]
    fn test_unblocked_issue_priority_boundaries() {
        for priority in [0, 1, 2, 3, 4] {
            let unblocked = UnblockedIssue {
                id: format!("bd-p{priority}"),
                title: format!("Priority {priority} task"),
                priority,
            };
            let json = serde_json::to_string(&unblocked).unwrap();
            let parsed: UnblockedIssue = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.priority, priority);
        }
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn test_close_result_multiple_closed_multiple_skipped() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-1".to_string(),
                    title: "Task 1".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: None,
                },
                ClosedIssue {
                    id: "bd-2".to_string(),
                    title: "Task 2".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:01Z".to_string(),
                    close_reason: Some("Batch close".to_string()),
                },
            ],
            skipped: vec![
                SkippedIssue {
                    id: "bd-3".to_string(),
                    reason: "issue not found".to_string(),
                },
                SkippedIssue {
                    id: "bd-4".to_string(),
                    reason: "already tombstone".to_string(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 2);
    }

    #[test]
    fn test_close_args_clone() {
        let args = CloseArgs {
            ids: vec!["bd-clone".to_string()],
            reason: Some("Clone test".to_string()),
            force: true,
            session: Some("sess".to_string()),
            suggest_next: true,
        };
        let cloned = args.clone();
        assert_eq!(cloned.ids, args.ids);
        assert_eq!(cloned.reason, args.reason);
        assert_eq!(cloned.force, args.force);
        assert_eq!(cloned.session, args.session);
        assert_eq!(cloned.suggest_next, args.suggest_next);
    }

    #[test]
    fn test_close_args_debug_impl() {
        let args = CloseArgs::default();
        let debug_str = format!("{args:?}");
        assert!(debug_str.contains("CloseArgs"));
        assert!(debug_str.contains("ids"));
        assert!(debug_str.contains("reason"));
    }
}
