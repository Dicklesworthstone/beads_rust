//! Defer and Undefer command implementations.

use crate::cli::{DeferArgs, UndeferArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::ReadyIssue;
use crate::model::{Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use crate::util::time::parse_flexible_timestamp;
use rich_rust::prelude::*;
use serde::Serialize;

/// Result of deferring a single issue (for text output).
#[derive(Debug, Serialize)]
pub struct DeferredIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<String>,
}

/// Issue that was skipped during defer.
#[derive(Debug, Serialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

/// Execute the defer command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_defer(
    args: &DeferArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let use_json = json || args.robot;

    tracing::info!("Executing defer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir(None)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let storage = &mut storage_ctx.storage;

    // Parse defer_until if provided
    let defer_until = args
        .until
        .as_ref()
        .map(|s| parse_flexible_timestamp(s, "defer_until"))
        .transpose()?;

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &args.ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    let mut deferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut deferred_full: Vec<Issue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, until = ?defer_until, "Deferring issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if already closed/tombstone
        if issue.status.is_terminal() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is terminal");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("cannot defer {} issue", issue.status.as_str()),
            });
            continue;
        }

        // Check if already deferred (with same time)
        if issue.status == Status::Deferred && issue.defer_until == defer_until {
            tracing::debug!(id = %id, "Issue already deferred with same time");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "already deferred".to_string(),
            });
            continue;
        }

        // Build update: set status=deferred, set defer_until
        let update = IssueUpdate {
            status: Some(Status::Deferred),
            defer_until: Some(defer_until),
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, defer_until = ?defer_until, "Issue deferred");

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        // Get updated issue for JSON output
        if let Some(updated) = storage.get_issue(id)? {
            deferred_full.push(updated);
        }

        deferred_issues.push(DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "deferred".to_string(),
            defer_until: defer_until.map(|dt| dt.to_rfc3339()),
        });
    }

    // Output
    if use_json {
        // bd outputs a bare array of updated issues
        let json_output: Vec<ReadyIssue> = deferred_full.iter().map(ReadyIssue::from).collect();
        let output = serde_json::to_string_pretty(&json_output).map_err(BeadsError::Json)?;
        println!("{output}");
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_defer_rich(&deferred_issues, &skipped_issues, ctx);
    } else {
        for deferred in &deferred_issues {
            print!("\u{23f1} Deferred {}: {}", deferred.id, deferred.title);
            if let Some(ref until) = deferred.defer_until {
                println!(" (until {until})");
            } else {
                println!(" (indefinitely)");
            }
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if deferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to defer.");
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

/// Execute the undefer command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_undefer(
    args: &UndeferArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let use_json = json || args.robot;

    tracing::info!("Executing undefer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir(None)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let storage = &mut storage_ctx.storage;

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &args.ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    let mut undeferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut undeferred_full: Vec<Issue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, "Undeferring issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if actually deferred (status or date)
        if issue.status != Status::Deferred && issue.defer_until.is_none() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is not deferred");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("not deferred (status: {})", issue.status.as_str()),
            });
            continue;
        }

        // Build update: set status=open, clear defer_until
        let update = IssueUpdate {
            status: Some(Status::Open),
            defer_until: Some(None), // Clear defer_until
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, "Issue undeferred");

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        // Get updated issue for JSON output
        if let Some(updated) = storage.get_issue(id)? {
            undeferred_full.push(updated);
        }

        undeferred_issues.push(DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "open".to_string(),
            defer_until: None,
        });
    }

    // Rebuild blocked cache since undeferred issues may become blockers
    if !undeferred_issues.is_empty() {
        tracing::info!(
            "Rebuilding blocked cache after undeferring {} issues",
            undeferred_issues.len()
        );
        storage.rebuild_blocked_cache(true)?;
    }

    // Output
    if use_json {
        // bd outputs a bare array of updated issues
        let json_output: Vec<ReadyIssue> = undeferred_full.iter().map(ReadyIssue::from).collect();
        let output = serde_json::to_string_pretty(&json_output).map_err(BeadsError::Json)?;
        println!("{output}");
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_undefer_rich(&undeferred_issues, &skipped_issues, ctx);
    } else {
        for undeferred in &undeferred_issues {
            println!(
                "\u{2713} Undeferred {}: {} (now open)",
                undeferred.id, undeferred.title
            );
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if undeferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to undefer.");
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render defer results with rich formatting.
fn render_defer_rich(deferred: &[DeferredIssue], skipped: &[SkippedIssue], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if deferred.is_empty() && skipped.is_empty() {
        content.append("No issues to defer.\n");
    } else {
        for item in deferred {
            content.append_styled("\u{23f1} ", theme.warning.clone());
            content.append_styled("Deferred ", theme.warning.clone());
            content.append_styled(&item.id, theme.emphasis.clone());
            content.append(": ");
            content.append(&item.title);
            content.append("\n");
            content.append_styled("  Status: ", theme.dimmed.clone());
            content.append_styled("open", theme.success.clone());
            content.append(" \u{2192} ");
            content.append_styled("deferred", theme.warning.clone());
            content.append("\n");
            if let Some(ref until) = item.defer_until {
                content.append_styled("  Until:  ", theme.dimmed.clone());
                content.append_styled(until, theme.accent.clone());
                content.append("\n");
            } else {
                content.append_styled("  Until:  ", theme.dimmed.clone());
                content.append_styled("indefinitely", theme.dimmed.clone());
                content.append("\n");
            }
        }

        for item in skipped {
            content.append_styled("\u{2298} ", theme.dimmed.clone());
            content.append_styled("Skipped ", theme.dimmed.clone());
            content.append_styled(&item.id, theme.emphasis.clone());
            content.append(": ");
            content.append_styled(&item.reason, theme.dimmed.clone());
            content.append("\n");
        }
    }

    let title = if deferred.len() == 1 && skipped.is_empty() {
        "Issue Deferred"
    } else {
        "Defer Results"
    };

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(title, theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render undefer results with rich formatting.
fn render_undefer_rich(
    undeferred: &[DeferredIssue],
    skipped: &[SkippedIssue],
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if undeferred.is_empty() && skipped.is_empty() {
        content.append("No issues to undefer.\n");
    } else {
        for item in undeferred {
            content.append_styled("\u{2713} ", theme.success.clone());
            content.append_styled("Undeferred ", theme.success.clone());
            content.append_styled(&item.id, theme.emphasis.clone());
            content.append(": ");
            content.append(&item.title);
            content.append("\n");
            content.append_styled("  Status: ", theme.dimmed.clone());
            content.append_styled("deferred", theme.warning.clone());
            content.append(" \u{2192} ");
            content.append_styled("open", theme.success.clone());
            content.append("\n");
        }

        for item in skipped {
            content.append_styled("\u{2298} ", theme.dimmed.clone());
            content.append_styled("Skipped ", theme.dimmed.clone());
            content.append_styled(&item.id, theme.emphasis.clone());
            content.append(": ");
            content.append_styled(&item.reason, theme.dimmed.clone());
            content.append("\n");
        }
    }

    let title = if undeferred.len() == 1 && skipped.is_empty() {
        "Issue Undeferred"
    } else {
        "Undefer Results"
    };

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(title, theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::{Datelike, Duration, Local, Utc};
    use std::env;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static TEST_DIR_LOCK: Mutex<()> = Mutex::new(());

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &std::path::Path) -> Self {
            let previous = env::current_dir().expect("current dir");
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    fn make_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            content_hash: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: None,
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
    fn test_parse_defer_time_rfc3339() {
        let result = parse_flexible_timestamp("2025-01-15T12:00:00Z", "defer_until").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 1);
        assert_eq!(result.day(), 15);
    }

    #[test]
    fn test_parse_defer_time_simple_date() {
        let result = parse_flexible_timestamp("2025-06-20", "defer_until").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 20);
    }

    #[test]
    fn test_parse_defer_time_relative_hours() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("+2h", "defer_until").unwrap();
        let after = Utc::now();

        // Result should be about 2 hours from now
        assert!(result > before + Duration::hours(1));
        assert!(result < after + Duration::hours(3));
    }

    #[test]
    fn test_parse_defer_time_relative_days() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("+1d", "defer_until").unwrap();
        let after = Utc::now();

        // Result should be about 1 day from now
        assert!(result > before + Duration::hours(23));
        assert!(result < after + Duration::hours(25));
    }

    #[test]
    fn test_parse_defer_time_relative_weeks() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("+1w", "defer_until").unwrap();
        let after = Utc::now();

        // Result should be about 1 week from now
        assert!(result > before + Duration::days(6));
        assert!(result < after + Duration::days(8));
    }

    #[test]
    fn test_parse_defer_time_tomorrow() {
        let result = parse_flexible_timestamp("tomorrow", "defer_until").unwrap();
        let expected_date = Local::now().date_naive() + Duration::days(1);

        // Check it's tomorrow (in UTC, might differ by a day due to timezone)
        let result_local = result.with_timezone(&Local);
        assert_eq!(result_local.date_naive(), expected_date);
    }

    #[test]
    fn test_parse_defer_time_next_week() {
        let result = parse_flexible_timestamp("next-week", "defer_until").unwrap();
        let expected_date = Local::now().date_naive() + Duration::weeks(1);

        let result_local = result.with_timezone(&Local);
        assert_eq!(result_local.date_naive(), expected_date);
    }

    #[test]
    fn test_parse_defer_time_invalid() {
        let result = parse_flexible_timestamp("invalid-time", "defer_until");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_defer_time_minutes() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("+30m", "defer_until").unwrap();
        let after = Utc::now();

        // Result should be about 30 minutes from now
        assert!(result > before + Duration::minutes(29));
        assert!(result < after + Duration::minutes(31));
    }

    #[test]
    fn test_parse_defer_time_negative() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("-1d", "defer_until").unwrap();
        let after = Utc::now();

        // Result should be about 1 day ago
        assert!(result < before - Duration::hours(23));
        assert!(result > after - Duration::hours(25));
    }

    #[test]
    fn execute_defer_sets_status_and_until() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-1", "Defer me");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let args = DeferArgs {
            ids: vec!["bd-defer-1".to_string()],
            until: Some("+1d".to_string()),
            robot: true,
        };
        execute_defer(&args, true, &CliOverrides::default(), &ctx).expect("defer");

        let updated = storage.get_issue("bd-defer-1").expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_some());
    }

    #[test]
    fn execute_defer_without_until_sets_indefinite() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-2", "Defer me later");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let args = DeferArgs {
            ids: vec!["bd-defer-2".to_string()],
            until: None,
            robot: true,
        };
        execute_defer(&args, true, &CliOverrides::default(), &ctx).expect("defer");

        let updated = storage.get_issue("bd-defer-2").expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_none());
    }

    #[test]
    fn execute_undefer_clears_defer_until() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-3", "Undefer me");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let defer_args = DeferArgs {
            ids: vec!["bd-defer-3".to_string()],
            until: Some("+1d".to_string()),
            robot: true,
        };
        execute_defer(&defer_args, true, &CliOverrides::default(), &ctx).expect("defer");

        let undefer_args = UndeferArgs {
            ids: vec!["bd-defer-3".to_string()],
            robot: true,
        };
        execute_undefer(&undefer_args, true, &CliOverrides::default(), &ctx).expect("undefer");

        let updated = storage.get_issue("bd-defer-3").expect("get").unwrap();
        assert_eq!(updated.status, Status::Open);
        assert!(updated.defer_until.is_none());
    }
}
