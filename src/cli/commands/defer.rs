//! Defer and Undefer command implementations.

use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_ids, update_issues_atomically_with_recovery,
};
use crate::cli::{DeferArgs, UndeferArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::{Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig};
use crate::util::time::parse_flexible_timestamp;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

/// Result of deferring a single issue (for text output).
#[derive(Debug, Clone, Serialize)]
pub struct DeferredIssue {
    pub id: String,
    pub title: String,
    pub previous_status: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<String>,
}

/// Issue that was skipped during defer.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
struct DeferResult {
    pub deferred: Vec<DeferredIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    #[serde(skip)]
    ordered_outcomes: Vec<DeferredOutcome>,
}

#[derive(Debug, Serialize)]
struct UndeferResult {
    pub undeferred: Vec<DeferredIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    #[serde(skip)]
    ordered_outcomes: Vec<DeferredOutcome>,
}

#[derive(Debug, Clone)]
enum DeferredOutcome {
    Changed(DeferredIssue),
    Skipped(SkippedIssue),
}

fn restored_status_after_undefer(issue: &Issue) -> Status {
    if issue.status == Status::Deferred {
        Status::Open
    } else {
        issue.status.clone()
    }
}

fn skipped_reason_text(reason: &str) -> String {
    sanitize_terminal_inline(reason).into_owned()
}

fn issue_id_text(id: &str) -> String {
    sanitize_terminal_inline(id).into_owned()
}

fn status_text(status: &str) -> String {
    sanitize_terminal_inline(status).into_owned()
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
    tracing::info!("Executing defer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let routed_batches = config::routing::group_issue_inputs_by_route(&args.ids, &beads_dir)?;
    let mut deferred_issues = Vec::new();
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

            let result = execute_defer_route(
                &batch_args,
                &batch_cli,
                ctx,
                &batch.beads_dir,
                batch.is_external,
            )?;
            routed_outcomes.push((batch.issue_inputs.clone(), result.ordered_outcomes));
            capacity_warnings.extend(result.warnings);
        }

        let ordered_outcomes =
            reorder_routed_items_by_requested_inputs(&args.ids, routed_outcomes, "defer routing")?;
        for outcome in ordered_outcomes {
            match outcome {
                DeferredOutcome::Changed(issue) => deferred_issues.push(issue),
                DeferredOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let result = execute_defer_route(args, cli, ctx, &beads_dir, false)?;
        deferred_issues = result.deferred;
        skipped_issues = result.skipped;
        capacity_warnings = result.warnings;
    }

    if let Some(last_deferred) = deferred_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_deferred.id);
    }

    render_defer_output(
        &deferred_issues,
        &skipped_issues,
        &capacity_warnings,
        args,
        json,
        ctx,
    )?;
    Ok(())
}

fn render_defer_output(
    deferred_issues: &[DeferredIssue],
    skipped_issues: &[SkippedIssue],
    capacity_warnings: &[crate::close_policy::WorkflowCapacityWarning],
    args: &DeferArgs,
    json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let use_structured_output = json || ctx.is_json() || ctx.is_toon() || args.robot;
    if use_structured_output {
        let result = DeferResult {
            deferred: deferred_issues.to_vec(),
            skipped: skipped_issues.to_vec(),
            warnings: capacity_warnings.to_vec(),
            ordered_outcomes: Vec::new(),
        };
        emit_structured_output(&result, ctx)?;
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_defer_rich(deferred_issues, skipped_issues, ctx);
        for warning in capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    } else {
        for deferred in deferred_issues {
            let id = issue_id_text(&deferred.id);
            print!(
                "\u{23f1} Deferred {}: {}",
                id,
                sanitize_terminal_inline(&deferred.title)
            );
            if let Some(ref until) = deferred.defer_until {
                println!(" (until {until})");
            } else {
                println!(" (indefinitely)");
            }
        }
        for skipped in skipped_issues {
            let id = issue_id_text(&skipped.id);
            let reason = skipped_reason_text(&skipped.reason);
            println!("\u{2298} Skipped {id}: {reason}");
        }
        if deferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to defer.");
        }
        for warning in capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_defer_route(
    args: &DeferArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<DeferResult> {
    let routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    // Reuse the routed authority for the storage open below; acquiring the
    // same database-family lock from a second descriptor in this process
    // would self-deadlock until the lock timeout (#409 routed cluster).
    let mut route_cli = cli.clone();
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let cli = &route_cli;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    let defer_until = args
        .until
        .as_ref()
        .map(|s| parse_flexible_timestamp(s, "defer_until"))
        .transpose()?;

    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    let mut deferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    let mut atomic_updates = Vec::new();
    let mut planned_changes = Vec::new();

    for id in &resolved_ids {
        let outcome_index = ordered_outcomes.len();
        ordered_outcomes.push(None);
        tracing::info!(id = %id, until = ?defer_until, "Deferring issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "defer",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status.is_terminal() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is terminal");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("cannot defer {} issue", issue.status.as_str()),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if issue.status == Status::Deferred && issue.defer_until == defer_until {
            tracing::debug!(id = %id, "Issue already deferred with same time");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "already deferred".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        let update = IssueUpdate {
            status: Some(Status::Deferred),
            defer_until: Some(defer_until),
            transition_comment: args.transition_comment.clone(),
            skip_cache_rebuild: true,
            ..Default::default()
        };

        atomic_updates.push((id.clone(), update));
        planned_changes.push((outcome_index, id.clone(), issue));
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
            "defer",
            &atomic_updates,
            &actor,
        );
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, false, "defer", update_result)?;
        capacity_warnings = storage_ctx.storage.take_capacity_warnings();
        cache_dirty = true;
    }

    for (outcome_index, id, issue) in planned_changes {
        tracing::info!(id = %id, defer_until = ?defer_until, "Issue deferred");

        let deferred = DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            previous_status: issue.status.as_str().to_string(),
            status: "deferred".to_string(),
            defer_until: defer_until.map(|dt| dt.to_rfc3339()),
        };
        ordered_outcomes[outcome_index] = Some(DeferredOutcome::Changed(deferred.clone()));
        deferred_issues.push(deferred);
    }

    let ordered_outcomes = ordered_outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| BeadsError::internal("defer batch outcome was not populated"))
        })
        .collect::<Result<Vec<_>>>()?;

    if cache_dirty {
        tracing::info!(
            "Rebuilding blocked cache after deferring {} issues",
            deferred_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "defer")?;
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

    Ok(DeferResult {
        deferred: deferred_issues,
        skipped: skipped_issues,
        warnings: capacity_warnings,
        ordered_outcomes,
    })
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
    tracing::info!("Executing undefer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let routed_batches = config::routing::group_issue_inputs_by_route(&args.ids, &beads_dir)?;
    let mut undeferred_issues = Vec::new();
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

            let result = execute_undefer_route(
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
            &args.ids,
            routed_outcomes,
            "undefer routing",
        )?;
        for outcome in ordered_outcomes {
            match outcome {
                DeferredOutcome::Changed(issue) => undeferred_issues.push(issue),
                DeferredOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let result = execute_undefer_route(args, cli, ctx, &beads_dir, false)?;
        undeferred_issues = result.undeferred;
        skipped_issues = result.skipped;
        capacity_warnings = result.warnings;
    }

    if let Some(last_undeferred) = undeferred_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_undeferred.id);
    }

    render_undefer_output(
        &undeferred_issues,
        &skipped_issues,
        &capacity_warnings,
        json,
        args,
        ctx,
    )?;
    Ok(())
}

fn render_undefer_output(
    undeferred_issues: &[DeferredIssue],
    skipped_issues: &[SkippedIssue],
    capacity_warnings: &[crate::close_policy::WorkflowCapacityWarning],
    json: bool,
    args: &UndeferArgs,
    ctx: &OutputContext,
) -> Result<()> {
    let use_structured_output = json || ctx.is_json() || ctx.is_toon() || args.robot;
    if use_structured_output {
        let result = UndeferResult {
            undeferred: undeferred_issues.to_vec(),
            skipped: skipped_issues.to_vec(),
            warnings: capacity_warnings.to_vec(),
            ordered_outcomes: Vec::new(),
        };
        emit_structured_output(&result, ctx)?;
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_undefer_rich(undeferred_issues, skipped_issues, ctx);
        for warning in capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    } else {
        for undeferred in undeferred_issues {
            let id = issue_id_text(&undeferred.id);
            let status = status_text(&undeferred.status);
            println!(
                "\u{2713} Undeferred {}: {} (now {})",
                id,
                sanitize_terminal_inline(&undeferred.title),
                status
            );
        }
        for skipped in skipped_issues {
            let id = issue_id_text(&skipped.id);
            let reason = skipped_reason_text(&skipped.reason);
            println!("\u{2298} Skipped {id}: {reason}");
        }
        if undeferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to undefer.");
        }
        for warning in capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_undefer_route(
    args: &UndeferArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<UndeferResult> {
    let routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    // Reuse the routed authority for the storage open below; acquiring the
    // same database-family lock from a second descriptor in this process
    // would self-deadlock until the lock timeout (#409 routed cluster).
    let mut route_cli = cli.clone();
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let cli = &route_cli;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    let mut undeferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    let mut atomic_updates = Vec::new();
    let mut planned_changes = Vec::new();

    for id in &resolved_ids {
        let outcome_index = ordered_outcomes.len();
        ordered_outcomes.push(None);
        tracing::info!(id = %id, "Undeferring issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "undefer",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status != Status::Deferred && issue.defer_until.is_none() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is not deferred");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("not deferred (status: {})", issue.status.as_str()),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if issue.status.is_terminal() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is terminal");
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("cannot undefer {} issue", issue.status.as_str()),
            };
            ordered_outcomes[outcome_index] = Some(DeferredOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        let restored_status = restored_status_after_undefer(&issue);
        let status_update = if issue.status == Status::Deferred {
            Some(Status::Open)
        } else {
            None
        };

        let update = IssueUpdate {
            transition_comment: status_update
                .as_ref()
                .and(args.transition_comment.as_ref())
                .cloned(),
            status: status_update,
            defer_until: Some(None),
            skip_cache_rebuild: true,
            ..Default::default()
        };

        atomic_updates.push((id.clone(), update));
        planned_changes.push((outcome_index, id.clone(), issue, restored_status));
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
            "undefer",
            &atomic_updates,
            &actor,
        );
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, false, "undefer", update_result)?;
        capacity_warnings = storage_ctx.storage.take_capacity_warnings();
        cache_dirty = true;
    }

    for (outcome_index, id, issue, restored_status) in planned_changes {
        tracing::info!(id = %id, "Issue undeferred");

        let undeferred = DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            previous_status: issue.status.as_str().to_string(),
            status: restored_status.as_str().to_string(),
            defer_until: None,
        };
        ordered_outcomes[outcome_index] = Some(DeferredOutcome::Changed(undeferred.clone()));
        undeferred_issues.push(undeferred);
    }

    let ordered_outcomes = ordered_outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| BeadsError::internal("undefer batch outcome was not populated"))
        })
        .collect::<Result<Vec<_>>>()?;

    if cache_dirty {
        tracing::info!(
            "Rebuilding blocked cache after undeferring {} issues",
            undeferred_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "undefer")?;
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

    Ok(UndeferResult {
        undeferred: undeferred_issues,
        skipped: skipped_issues,
        warnings: capacity_warnings,
        ordered_outcomes,
    })
}

fn emit_structured_output<T: Serialize>(payload: &T, ctx: &OutputContext) -> Result<()> {
    if ctx.is_toon() {
        ctx.toon(payload);
    } else if ctx.is_json() {
        ctx.json_pretty(payload);
    } else {
        let json = serde_json::to_string_pretty(payload)?;
        println!("{json}");
    }
    Ok(())
}

fn reorder_routed_items_by_requested_inputs<T>(
    requested_inputs: &[String],
    routed_items: Vec<(Vec<String>, Vec<T>)>,
    context: &str,
) -> Result<Vec<T>> {
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
                let input = issue_id_text(&input);
                return Err(BeadsError::internal(format!(
                    "{context} returned unexpected issue input {input}"
                )));
            };
            let Some(slot) = ordered_items.get_mut(index) else {
                let input = issue_id_text(&input);
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
                    .map(|input| issue_id_text(input))
                    .unwrap_or_else(|| "<unknown>".to_string());
                BeadsError::internal(format!("{context} did not produce a result for {input}"))
            })
        })
        .collect()
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
            let id = issue_id_text(&item.id);
            content.append_styled("\u{23f1} ", theme.warning.clone());
            content.append_styled("Deferred ", theme.warning.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append(sanitize_terminal_inline(&item.title).as_ref());
            content.append("\n");
            content.append_styled("  Status: ", theme.dimmed.clone());
            let previous_status = status_text(&item.previous_status);
            content.append_styled(&previous_status, theme.success.clone());
            content.append(" \u{2192} ");
            content.append_styled("deferred", theme.warning.clone());
            content.append("\n");
            content.append_styled("  Until:  ", theme.dimmed.clone());
            if let Some(ref until) = item.defer_until {
                content.append_styled(until, theme.accent.clone());
            } else {
                content.append_styled("indefinitely", theme.dimmed.clone());
            }
            content.append("\n");
        }

        for item in skipped {
            let id = issue_id_text(&item.id);
            let reason = skipped_reason_text(&item.reason);
            content.append_styled("\u{2298} ", theme.dimmed.clone());
            content.append_styled("Skipped ", theme.dimmed.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append_styled(&reason, theme.dimmed.clone());
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
            let id = issue_id_text(&item.id);
            content.append_styled("\u{2713} ", theme.success.clone());
            content.append_styled("Undeferred ", theme.success.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append(sanitize_terminal_inline(&item.title).as_ref());
            content.append("\n");
            content.append_styled("  Status: ", theme.dimmed.clone());
            let previous_status = status_text(&item.previous_status);
            let status = status_text(&item.status);
            content.append_styled(&previous_status, theme.warning.clone());
            if item.previous_status == item.status {
                content.append_styled(" (unchanged)", theme.dimmed.clone());
            } else {
                content.append(" \u{2192} ");
                content.append_styled(&status, theme.success.clone());
            }
            content.append("\n");
        }

        for item in skipped {
            let id = issue_id_text(&item.id);
            let reason = skipped_reason_text(&item.reason);
            content.append_styled("\u{2298} ", theme.dimmed.clone());
            content.append_styled("Skipped ", theme.dimmed.clone());
            content.append_styled(&id, theme.emphasis.clone());
            content.append(": ");
            content.append_styled(&reason, theme.dimmed.clone());
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

    use tempfile::TempDir;

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
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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

    fn deferred_output_item(id: &str) -> DeferredIssue {
        DeferredIssue {
            id: id.to_string(),
            title: "Deferred output".to_string(),
            previous_status: "open".to_string(),
            status: "deferred".to_string(),
            defer_until: None,
        }
    }

    #[test]
    fn skipped_reason_text_sanitizes_terminal_controls() {
        let reason = skipped_reason_text("bad\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!reason.chars().any(char::is_control));
        assert!(reason.contains("\\u{1b}[2J"));
        assert!(reason.contains("\\r"));
        assert!(reason.contains("\\u{8}"));
        assert!(reason.contains("\\n"));
        assert!(reason.contains("\\u{7}"));
        assert!(reason.contains("\\u{9b}"));
    }

    #[test]
    fn issue_id_text_sanitizes_terminal_controls() {
        let id = issue_id_text("bd-bad\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!id.chars().any(char::is_control));
        assert!(id.contains("\\u{1b}[2J"));
        assert!(id.contains("\\r"));
        assert!(id.contains("\\u{8}"));
        assert!(id.contains("\\n"));
        assert!(id.contains("\\u{7}"));
        assert!(id.contains("\\u{9b}"));
    }

    #[test]
    fn status_text_sanitizes_terminal_controls() {
        let status = status_text("qa\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!status.chars().any(char::is_control));
        assert!(status.contains("\\u{1b}[2J"));
        assert!(status.contains("\\r"));
        assert!(status.contains("\\u{8}"));
        assert!(status.contains("\\n"));
        assert!(status.contains("\\u{7}"));
        assert!(status.contains("\\u{9b}"));
    }

    #[test]
    fn defer_result_serializes_as_object_without_skips() {
        let result = DeferResult {
            deferred: vec![deferred_output_item("bd-defer-1")],
            skipped: Vec::new(),
            ordered_outcomes: Vec::new(),
            warnings: Vec::new(),
        };

        let value = serde_json::to_value(result).expect("serialize defer result");

        assert!(value.is_object(), "defer result should be an object");
        assert_eq!(value["deferred"][0]["id"], "bd-defer-1");
        assert!(
            value.get("skipped").is_none(),
            "empty skipped list should stay omitted"
        );
    }

    #[test]
    fn undefer_result_serializes_as_object_without_skips() {
        let result = UndeferResult {
            undeferred: vec![DeferredIssue {
                status: "open".to_string(),
                ..deferred_output_item("bd-undefer-1")
            }],
            skipped: Vec::new(),
            ordered_outcomes: Vec::new(),
            warnings: Vec::new(),
        };

        let value = serde_json::to_value(result).expect("serialize undefer result");

        assert!(value.is_object(), "undefer result should be an object");
        assert_eq!(value["undeferred"][0]["id"], "bd-undefer-1");
        assert!(
            value.get("skipped").is_none(),
            "empty skipped list should stay omitted"
        );
    }

    #[test]
    fn reorder_routed_items_preserves_duplicate_requested_inputs() -> Result<()> {
        let requested = vec![
            "bd-a".to_string(),
            "bd-b".to_string(),
            "bd-a".to_string(),
            "bd-c".to_string(),
        ];
        let routed_items = vec![
            (
                vec!["bd-a".to_string(), "bd-a".to_string()],
                vec!["first-a", "second-a"],
            ),
            (vec!["bd-b".to_string(), "bd-c".to_string()], vec!["b", "c"]),
        ];

        let ordered =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "test routing")?;

        assert_eq!(ordered, vec!["first-a", "b", "second-a", "c"]);
        Ok(())
    }

    #[test]
    fn reorder_routed_items_sanitizes_missing_input_error() {
        let requested = vec!["bd-a\x1b[2J\nbad".to_string(), "bd-b".to_string()];
        let routed_items = vec![(vec!["bd-b".to_string()], vec!["b"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "test routing")
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
        let requested = vec!["bd-a".to_string()];
        let routed_items = vec![(vec!["bd-b\x1b[2J\nbad".to_string()], vec!["b"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "test routing")
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
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue_id = format!(
            "{}-defer-1",
            storage
                .get_config("issue_prefix")
                .expect("prefix config")
                .expect("workspace prefix")
        );
        let issue = make_issue(&issue_id, "Defer me");
        storage.create_issue(&issue, "tester").expect("create");

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let args = DeferArgs {
            ids: vec![issue_id.clone()],
            until: Some("+1d".to_string()),
            robot: true,
            ..Default::default()
        };
        execute_defer(&args, true, &cli, &ctx).expect("defer");

        let updated = storage.get_issue(&issue_id).expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_some());
    }

    #[test]
    fn execute_defer_without_until_sets_indefinite() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue_id = format!(
            "{}-defer-2",
            storage
                .get_config("issue_prefix")
                .expect("prefix config")
                .expect("workspace prefix")
        );
        let issue = make_issue(&issue_id, "Defer me later");
        storage.create_issue(&issue, "tester").expect("create");

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let args = DeferArgs {
            ids: vec![issue_id.clone()],
            until: None,
            robot: true,
            ..Default::default()
        };
        execute_defer(&args, true, &cli, &ctx).expect("defer");

        let updated = storage.get_issue(&issue_id).expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_none());
    }

    #[test]
    fn execute_undefer_clears_defer_until() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let issue_id = {
            let storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
            format!(
                "{}-defer-3",
                storage
                    .get_config("issue_prefix")
                    .expect("prefix config")
                    .expect("workspace prefix")
            )
        };
        {
            let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
            let issue = make_issue(&issue_id, "Undefer me");
            storage.create_issue(&issue, "tester").expect("create");
        }

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let defer_args = DeferArgs {
            ids: vec![issue_id.clone()],
            until: Some("+1d".to_string()),
            robot: true,
            ..Default::default()
        };
        execute_defer(&defer_args, true, &cli, &ctx).expect("defer");

        let undefer_args = UndeferArgs {
            ids: vec![issue_id.clone()],
            robot: true,
            ..Default::default()
        };
        execute_undefer(&undefer_args, true, &cli, &ctx).expect("undefer");

        let storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("reopen");
        let updated = storage.get_issue(&issue_id).expect("get").unwrap();
        assert_eq!(updated.status, Status::Open);
        assert!(updated.defer_until.is_none());
    }

    #[test]
    fn execute_undefer_preserves_non_deferred_status_for_soft_defer() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue_id = format!(
            "{}-soft-defer-1",
            storage
                .get_config("issue_prefix")
                .expect("prefix config")
                .expect("workspace prefix")
        );
        let mut issue = make_issue(&issue_id, "Soft defer in progress");
        issue.status = Status::InProgress;
        issue.defer_until = Some(Utc::now() + Duration::days(1));
        storage.create_issue(&issue, "tester").expect("create");

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let undefer_args = UndeferArgs {
            ids: vec![issue_id.clone()],
            robot: true,
            ..Default::default()
        };
        execute_undefer(&undefer_args, true, &cli, &ctx).expect("undefer");

        let updated = storage.get_issue(&issue_id).expect("get").unwrap();
        assert_eq!(updated.status, Status::InProgress);
        assert!(updated.defer_until.is_none());
    }

    #[test]
    fn execute_undefer_skips_terminal_issue_with_stale_defer_until() -> Result<()> {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new()?;
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx)?;

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db"))?;
        let issue_id = format!(
            "{}-terminal-defer-1",
            storage
                .get_config("issue_prefix")?
                .ok_or_else(|| BeadsError::internal("workspace prefix missing"))?
        );
        let mut issue = make_issue(&issue_id, "Closed with stale defer date");
        issue.status = Status::Closed;
        issue.closed_at = Some(Utc::now());
        issue.defer_until = Some(Utc::now() + Duration::days(1));
        storage.create_issue(&issue, "tester")?;

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let undefer_args = UndeferArgs {
            ids: vec![issue_id.clone()],
            robot: true,
            ..Default::default()
        };
        execute_undefer(&undefer_args, true, &cli, &ctx)?;

        let updated = storage
            .get_issue(&issue_id)?
            .ok_or_else(|| BeadsError::IssueNotFound {
                id: issue_id.clone(),
            })?;
        assert_eq!(updated.status, Status::Closed);
        assert!(updated.defer_until.is_some());

        Ok(())
    }
}
