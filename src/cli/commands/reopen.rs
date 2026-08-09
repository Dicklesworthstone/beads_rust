//! Reopen command implementation.

use crate::cli::ReopenArgs;
use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_ids, update_issues_atomically_with_recovery,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::Status;
use crate::output::{OutputContext, OutputMode};
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

/// Result of reopening a single issue.
#[derive(Debug, Clone, Serialize)]
pub struct ReopenedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

/// Issue that was skipped during reopen.
#[derive(Debug, Clone, Serialize)]
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    #[serde(skip)]
    ordered_outcomes: Vec<ReopenOutcome>,
}

#[derive(Debug, Clone)]
enum ReopenOutcome {
    Reopened(ReopenedIssue),
    Skipped(SkippedIssue),
}

/// Execute the reopen command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &ReopenArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let use_structured_output = json || ctx.is_json() || ctx.is_toon() || args.robot;

    tracing::info!("Executing reopen command");

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut target_inputs = args.ids.clone();
    if target_inputs.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_inputs.push(last_touched);
    }

    let routed_batches = config::routing::group_issue_inputs_by_route(&target_inputs, &beads_dir)?;
    let mut reopened_issues = Vec::new();
    let mut skipped_issues = Vec::new();
    let mut capacity_warnings = Vec::new();

    if routed_batches.iter().any(|batch| batch.is_external) {
        let normalized_local_beads_dir =
            dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let mut routed_outcomes = Vec::new();

        for batch in routed_batches {
            let mut batch_args = args.clone();
            batch_args.ids.clone_from(&batch.issue_inputs);

            let normalized_batch_beads_dir =
                dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
            let mut batch_cli = cli.clone();
            batch_cli.db = if normalized_batch_beads_dir == normalized_local_beads_dir {
                cli.db.clone()
            } else {
                None
            };

            let result = execute_route(
                &batch_args,
                &batch_cli,
                ctx,
                &batch.beads_dir,
                batch.is_external,
            )?;
            routed_outcomes.push((batch.issue_inputs.clone(), result.ordered_outcomes));
            capacity_warnings.extend(result.warnings);
        }

        let ordered_outcomes = reorder_routed_items_by_requested_inputs(
            &target_inputs,
            routed_outcomes,
            "reopen routing",
        )?;
        for outcome in ordered_outcomes {
            match outcome {
                ReopenOutcome::Reopened(issue) => reopened_issues.push(issue),
                ReopenOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let mut local_args = args.clone();
        local_args.ids = target_inputs;
        let result = execute_route(&local_args, cli, ctx, &beads_dir, false)?;
        reopened_issues = result.reopened;
        skipped_issues = result.skipped;
        capacity_warnings = result.warnings;
    }

    if let Some(last_reopened) = reopened_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_reopened.id);
    }

    if use_structured_output {
        let result = ReopenResult {
            reopened: reopened_issues,
            skipped: skipped_issues,
            warnings: capacity_warnings,
            ordered_outcomes: Vec::new(),
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else if ctx.is_json() {
            ctx.json_pretty(&result);
        } else {
            let json_ctx = OutputContext::from_flags(true, false, true);
            json_ctx.json_pretty(&result);
        }
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_reopen_rich(
            &reopened_issues,
            &skipped_issues,
            args.reason.as_deref(),
            ctx,
        );
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    } else {
        for reopened in &reopened_issues {
            let id = reopen_issue_id_text(&reopened.id);
            print!(
                "\u{2713} Reopened {}: {}",
                id,
                sanitize_terminal_inline(&reopened.title)
            );
            if let Some(ref reason) = args.reason {
                println!("{}", reopen_reason_suffix(reason));
            } else {
                println!();
            }
        }
        for skipped in &skipped_issues {
            let id = reopen_issue_id_text(&skipped.id);
            println!(
                "\u{2298} Skipped {}: {}",
                id,
                sanitize_terminal_inline(&skipped.reason)
            );
        }
        if reopened_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to reopen.");
        }
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_route(
    args: &ReopenArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<ReopenResult> {
    let _routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    let mut reopened_issues: Vec<ReopenedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    let mut atomic_updates = Vec::new();
    let mut planned_reopens = Vec::new();

    for id in &resolved_ids {
        let outcome_index = ordered_outcomes.len();
        ordered_outcomes.push(None);
        tracing::info!(id = %id, "Reopening issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "reopen",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(ReopenOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status == Status::Tombstone {
            tracing::debug!(id = %id, "Issue is tombstoned and cannot be reopened");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "cannot reopen tombstone issue".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(ReopenOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if issue.status != Status::Closed {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is not closed");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
            };
            ordered_outcomes[outcome_index] = Some(ReopenOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        tracing::debug!(previous_status = ?issue.status, "Issue was previously {:?}", issue.status);

        let update = IssueUpdate {
            status: Some(Status::Open),
            closed_at: Some(None),
            close_reason: Some(None),
            closed_by_session: Some(None),
            defer_until: Some(None),
            deleted_at: Some(None),
            deleted_by: Some(None),
            delete_reason: Some(None),
            transition_comment: args
                .reason
                .as_ref()
                .map(|reason| format!("Reopened: {reason}")),
            skip_cache_rebuild: true,
            ..Default::default()
        };

        atomic_updates.push((id.clone(), update));
        planned_reopens.push((outcome_index, id.clone(), issue));
    }

    let mut capacity_warnings = Vec::new();
    if !atomic_updates.is_empty() {
        storage_ctx
            .storage
            .set_pending_event_attribution(crate::storage::EventAttribution::new(
                args.agent_name.as_deref(),
                args.harness.as_deref(),
                args.model.as_deref(),
                super::session_attribution_from_env().as_deref(),
            ));
        let update_result = update_issues_atomically_with_recovery(
            &mut storage_ctx,
            true,
            "reopen",
            &atomic_updates,
            &actor,
        );
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, false, "reopen", update_result)?;
        capacity_warnings = storage_ctx.storage.take_capacity_warnings();
        cache_dirty = true;
    }

    for (outcome_index, id, issue) in planned_reopens {
        tracing::info!(id = %id, reason = ?args.reason, "Issue reopened");

        let reopened = ReopenedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "open".to_string(),
            closed_at: None,
        };
        ordered_outcomes[outcome_index] = Some(ReopenOutcome::Reopened(reopened.clone()));
        reopened_issues.push(reopened);
    }

    let ordered_outcomes = ordered_outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| BeadsError::internal("reopen batch outcome was not populated"))
        })
        .collect::<Result<Vec<_>>>()?;

    if cache_dirty {
        tracing::info!(
            "Rebuilding blocked cache after reopening {} issues",
            reopened_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "reopen")?;
    }

    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(ReopenResult {
        reopened: reopened_issues,
        skipped: skipped_issues,
        warnings: capacity_warnings,
        ordered_outcomes,
    })
}

fn reorder_routed_items_by_requested_inputs<T>(
    requested_inputs: &[String],
    routed_items: Vec<(Vec<String>, Vec<T>)>,
    context: &str,
) -> Result<Vec<T>> {
    fn issue_input_text(input: &str) -> String {
        sanitize_terminal_inline(input).into_owned()
    }

    let mut positions_by_input: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, input) in requested_inputs.iter().enumerate() {
        positions_by_input
            .entry(input.as_str())
            .or_default()
            .push_back(index);
    }

    let mut ordered_items: Vec<Option<T>> = (0..requested_inputs.len()).map(|_| None).collect();
    for (batch_inputs, batch_items) in routed_items {
        if batch_inputs.len() != batch_items.len() {
            return Err(BeadsError::internal(format!(
                "{context} produced mismatched issue/result counts"
            )));
        }

        for (input, item) in batch_inputs.into_iter().zip(batch_items) {
            let Some(index) = positions_by_input
                .get_mut(input.as_str())
                .and_then(VecDeque::pop_front)
            else {
                let input = issue_input_text(&input);
                return Err(BeadsError::internal(format!(
                    "{context} returned unexpected issue input {input}"
                )));
            };
            let Some(slot) = ordered_items.get_mut(index) else {
                let input = issue_input_text(&input);
                return Err(BeadsError::internal(format!(
                    "{context} returned out-of-range issue input {input}"
                )));
            };
            *slot = Some(item);
        }
    }

    ordered_items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.ok_or_else(|| {
                let input = requested_inputs
                    .get(index)
                    .map(|input| issue_input_text(input))
                    .unwrap_or_else(|| "<unknown>".to_string());
                BeadsError::internal(format!("{context} did not produce a result for {input}"))
            })
        })
        .collect()
}

fn reopen_reason_suffix(reason: &str) -> String {
    format!(" ({})", sanitize_terminal_inline(reason))
}

fn reopen_issue_id_text(id: &str) -> String {
    sanitize_terminal_inline(id).into_owned()
}

/// Render reopen results with rich formatting.
fn render_reopen_rich(
    reopened: &[ReopenedIssue],
    skipped: &[SkippedIssue],
    reason: Option<&str>,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if reopened.is_empty() && skipped.is_empty() {
        content.append("No issues to reopen.\n");
    } else {
        for item in reopened {
            let id = reopen_issue_id_text(&item.id);
            content.append_styled("\u{2713} ", theme.success.clone());
            content.append_styled("Reopened ", theme.success.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append(sanitize_terminal_inline(&item.title).as_ref());
            if let Some(r) = reason {
                content.append_styled(&reopen_reason_suffix(r), theme.dimmed.clone());
            }
            content.append("\n");
            content.append_styled("  Status: ", theme.dimmed.clone());
            content.append_styled("closed", theme.error.clone());
            content.append(" \u{2192} ");
            content.append_styled("open", theme.success.clone());
            content.append("\n");
        }

        for item in skipped {
            let id = reopen_issue_id_text(&item.id);
            content.append_styled("\u{2298} ", theme.warning.clone());
            content.append_styled("Skipped ", theme.warning.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append_styled(
                sanitize_terminal_inline(&item.reason).as_ref(),
                theme.dimmed.clone(),
            );
            content.append("\n");
        }
    }

    let title = if reopened.len() == 1 && skipped.is_empty() {
        "Issue Reopened"
    } else {
        "Reopen Results"
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
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use chrono::{Duration, Utc};

    use tempfile::TempDir;

    fn make_closed_deferred_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Closed,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            closed_at: Some(now),
            defer_until: Some(now + Duration::days(7)),
            ..Issue::default()
        }
    }

    #[test]
    fn execute_clears_defer_until_when_reopening_closed_deferred_issue() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, None, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(
                &make_closed_deferred_issue("bd-reopen-deferred", "Closed deferred issue"),
                "tester",
            )
            .expect("create issue");
        drop(storage);

        let args = ReopenArgs {
            ids: vec!["bd-reopen-deferred".to_string()],
            reason: None,
            robot: false,
            ..Default::default()
        };
        let overrides = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };
        execute(&args, false, &overrides, &ctx).expect("reopen");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let issue = storage
            .get_issue("bd-reopen-deferred")
            .expect("get issue")
            .expect("issue exists");

        assert_eq!(issue.status, Status::Open);
        assert!(issue.defer_until.is_none());
        assert!(issue.closed_at.is_none());
    }

    #[test]
    fn execute_reopen_tombstone_skips_without_resurrecting_it() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, None, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let issue = Issue {
            id: "bd-reopen-tombstone".to_string(),
            title: "Deleted issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ..Issue::default()
        };
        storage
            .create_issue(&issue, "tester")
            .expect("create issue");
        storage
            .delete_issue(
                "bd-reopen-tombstone",
                "tester",
                "delete for reopen test",
                None,
            )
            .expect("delete issue");
        drop(storage);

        let args = ReopenArgs {
            ids: vec!["bd-reopen-tombstone".to_string()],
            reason: None,
            robot: false,
            ..Default::default()
        };
        let overrides = CliOverrides {
            db: Some(db_path.clone()),
            ..CliOverrides::default()
        };
        execute(&args, false, &overrides, &ctx).expect("reopen");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let issue = storage
            .get_issue("bd-reopen-tombstone")
            .expect("get issue")
            .expect("issue exists");

        assert_eq!(issue.status, Status::Tombstone);
        assert!(issue.deleted_at.is_some());
    }

    #[test]
    fn reopen_reason_suffix_sanitizes_terminal_controls() {
        let suffix = reopen_reason_suffix("bad\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(suffix.starts_with(" (bad"));
        assert!(suffix.ends_with(')'));
        assert!(!suffix.chars().any(char::is_control));
        assert!(suffix.contains("\\u{1b}[2J"));
        assert!(suffix.contains("\\r"));
        assert!(suffix.contains("\\u{8}"));
        assert!(suffix.contains("\\n"));
        assert!(suffix.contains("\\u{7}"));
        assert!(suffix.contains("\\u{9b}"));
    }

    #[test]
    fn reopen_issue_id_text_sanitizes_terminal_controls() {
        let id = reopen_issue_id_text("bd-bad\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!id.chars().any(char::is_control));
        assert!(id.contains("\\u{1b}[2J"));
        assert!(id.contains("\\r"));
        assert!(id.contains("\\u{8}"));
        assert!(id.contains("\\n"));
        assert!(id.contains("\\u{7}"));
        assert!(id.contains("\\u{9b}"));
    }

    #[test]
    fn reorder_routed_items_sanitizes_missing_input_error() {
        let requested = vec!["bd-reopen\x1b[2J\nbad".to_string(), "bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-ok".to_string()], vec!["ok"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "reopen routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Internal { .. }),
            "unexpected error: {err:?}"
        );
        if let BeadsError::Internal { message } = err {
            assert!(!message.chars().any(char::is_control));
            assert!(message.contains("\\u{1b}[2J"));
            assert!(message.contains("\\n"));
        }
    }

    #[test]
    fn reorder_routed_items_sanitizes_unexpected_input_error() {
        let requested = vec!["bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-reopen\x1b[2J\nbad".to_string()], vec!["bad"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "reopen routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Internal { .. }),
            "unexpected error: {err:?}"
        );
        if let BeadsError::Internal { message } = err {
            assert!(!message.chars().any(char::is_control));
            assert!(message.contains("\\u{1b}[2J"));
            assert!(message.contains("\\n"));
        }
    }
}
