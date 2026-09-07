//! Close command implementation.

use crate::cli::CloseArgs as CliCloseArgs;
use crate::cli::commands::{
    RouteResources, acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    finalize_batched_blocked_cache_refresh, preserve_blocked_cache_on_error,
    report_auto_flush_failure, resolve_issue_ids, same_workspace,
    update_issues_atomically_with_recovery,
};
use crate::close_policy::{
    self, AttributionTier, AttributionValues, CloseEvidence, ClosePolicy, PolicyDocument,
    PolicyViolation,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::{Issue, IssueType, Status};
use crate::output::OutputContext;
use crate::storage::{EventAttribution, IssueUpdate, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// Internal arguments for the close command.
#[derive(Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close
    pub ids: Vec<String>,
    /// Close reason
    pub reason: Option<String>,
    /// New comment committed atomically with the close transition.
    pub transition_comment: Option<String>,
    /// Force close even if blocked
    pub force: bool,
    /// Session ID for `closed_by_session` field
    pub session: Option<String>,
    /// Return newly unblocked issues (single ID only)
    pub suggest_next: bool,
    /// Tier 1 attribution: agent name (issue #274 Phase 1).
    pub agent_name: Option<String>,
    /// Tier 1 attribution: harness identifier.
    pub harness: Option<String>,
    /// Tier 1 attribution: model identifier.
    pub model: Option<String>,
    /// Bypass closure-time policy gates.
    pub bypass_policy: bool,
    /// Reason for bypass. Required when `bypass_policy = true`.
    pub bypass_reason: Option<String>,
}

impl From<&CliCloseArgs> for CloseArgs {
    fn from(cli: &CliCloseArgs) -> Self {
        Self {
            ids: cli.ids.clone(),
            reason: cli.reason.clone(),
            transition_comment: cli.transition_comment.clone(),
            force: cli.force,
            session: cli.session.clone(),
            suggest_next: cli.suggest_next,
            agent_name: cli.agent_name.clone(),
            harness: cli.harness.clone(),
            model: cli.model.clone(),
            bypass_policy: cli.bypass_policy,
            bypass_reason: cli.bypass_reason.clone(),
        }
    }
}

/// Aggregate of policy gates that fired for a single candidate close.
pub(crate) struct EvaluatedGates {
    pub(crate) violations: Vec<PolicyViolation>,
}

/// Validate the `--bypass-policy` / `--bypass-reason` flag pair before
/// touching storage. Mirrors the documented contract: bypass requires a
/// non-empty reason and is meaningless without `--bypass-policy`.
fn validate_bypass_args(args: &CloseArgs) -> Result<()> {
    if args.bypass_policy {
        let reason_present = args
            .bypass_reason
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if !reason_present {
            return Err(BeadsError::validation(
                "bypass-reason",
                "--bypass-policy requires --bypass-reason \"<text>\"",
            ));
        }
    } else if args
        .bypass_reason
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
    {
        return Err(BeadsError::validation(
            "bypass-policy",
            "--bypass-reason was set without --bypass-policy",
        ));
    }
    Ok(())
}

/// Resolve attribution values for the close. CLI flags take precedence over
/// env vars; both are ignored when the policy.yaml `attribution.tier` is
/// `off`. Tier 2/3 ("require"/"allowlist") are out of scope for Phase 1.
pub(crate) fn resolve_attribution_for_close(
    args: &CloseArgs,
    policy_doc: &PolicyDocument,
) -> AttributionValues {
    if policy_doc.close_policy.attribution.tier == AttributionTier::Off {
        return AttributionValues::default();
    }
    AttributionValues::resolve_from_env(
        args.agent_name.as_deref(),
        args.harness.as_deref(),
        args.model.as_deref(),
    )
}

/// Run every enabled gate against `issue` and produce the (possibly empty)
/// violation list. This includes the close-policy gates (issue #274) *and* the
/// workflow gate engine (issue #312, layer 2): closing an issue is a transition
/// into the `closed` state, so any `workflow.gates` rule guarding
/// `"<current> -> closed"` is enforced here too.
pub(crate) fn evaluate_close_policy(
    policy: &ClosePolicy,
    workflow: &crate::close_policy::Workflow,
    storage: &SqliteStorage,
    issue_id: &str,
    issue: &Issue,
    args: &CloseArgs,
    close_actor: &str,
) -> Result<EvaluatedGates> {
    // Look up the in_progress actor only when the gate is enabled — this
    // saves a query per close for repos that don't enable that specific
    // gate.
    let in_progress_actor = if policy.forbid_self_close_after_in_progress.enabled {
        storage.find_last_in_progress_actor(issue_id)?
    } else {
        None
    };

    let evidence = CloseEvidence {
        issue_id,
        close_reason: args.reason.as_deref(),
        description: issue.description.as_deref(),
        design: issue.design.as_deref(),
        acceptance_criteria: issue.acceptance_criteria.as_deref(),
        notes: issue.notes.as_deref(),
        close_actor,
        in_progress_actor: in_progress_actor.as_deref(),
    };

    let mut violations = close_policy::evaluate(policy, &evidence);

    // Deferred-dependents gate (beads_rust#303). Storage-backed, so it lives
    // here rather than in the pure `close_policy::evaluate`. We only query when
    // the gate is enabled to avoid a dependency lookup per close otherwise.
    if policy.forbid_close_with_deferred_dependents.enabled {
        let deferred_dependents = collect_deferred_blocks_dependents(storage, issue_id)?;
        if let Some(violation) =
            close_policy::deferred_dependents_violation(issue_id, &deferred_dependents)
        {
            violations.push(violation);
        }
    }

    // The storage transaction is the canonical gate/required-field chokepoint.
    // Pre-evaluate these rules only for an explicit bypass so the warning and
    // close metadata can name exactly what the operator bypassed without
    // introducing a read-before-write race on the ordinary path.
    if args.bypass_policy {
        let from = issue.status.as_str();
        let to = Status::Closed.as_str();
        violations.extend(close_policy::evaluate_transition_required_fields(
            workflow,
            issue_id,
            Some(from),
            to,
            issue.acceptance_criteria.as_deref(),
            args.transition_comment.as_deref(),
        ));
        if workflow.gates_enforced() && workflow.gate_rule_for(from, to).is_some() {
            let labels = storage.get_labels(issue_id)?;
            let results = storage.get_scoped_gate_results(issue_id, from, to)?;
            violations.extend(close_policy::evaluate_gates(
                workflow,
                issue_id,
                from,
                to,
                &labels,
                issue.priority.0,
                &results,
            ));
        }
    }

    Ok(EvaluatedGates { violations })
}

/// Collect the IDs of issues that have a `blocks` edge *from* `issue_id`
/// (i.e. depend on `issue_id` as a prerequisite) and are currently in
/// `deferred` status. Used by the `forbid_close_with_deferred_dependents`
/// gate (beads_rust#303).
///
/// Edge direction: a `blocks` dependency row
/// `(issue_id=DEP, depends_on_id=PREREQ)` means "PREREQ blocks DEP" / "DEP
/// depends on PREREQ". `get_dependents_with_metadata` returns the `DEP` rows
/// for a given `PREREQ`, which is exactly the set we filter.
fn collect_deferred_blocks_dependents(
    storage: &SqliteStorage,
    issue_id: &str,
) -> Result<Vec<String>> {
    let dependents = storage.get_dependents_with_metadata(issue_id)?;
    Ok(dependents
        .into_iter()
        .filter(|dependent| {
            dependent.dep_type == crate::model::DependencyType::Blocks.as_str()
                && dependent.status == Status::Deferred
        })
        .map(|dependent| dependent.id)
        .collect())
}

fn summarize_violations(violations: &[PolicyViolation]) -> String {
    if let [single] = violations {
        return single.message.clone();
    }
    let lines: Vec<String> = violations
        .iter()
        .map(|v| format!("- {}", v.message))
        .collect();
    format!("{} gates fired:\n{}", violations.len(), lines.join("\n"))
}

fn emit_bypass_warning(ctx: &OutputContext, issue_id: &str, violations: &[PolicyViolation]) {
    let summary = summarize_violations(violations);
    let id = sanitize_terminal_inline(issue_id);
    let summary = sanitize_terminal_inline(&summary);
    ctx.warning(&format!(
        "Closing {} despite policy violation(s) (--bypass-policy): {}",
        id.as_ref(),
        summary.as_ref()
    ));
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
    execute_cli_with_storage(cli_args, json, cli, ctx, &mut None)
}

/// Execute the close command from CLI arguments, reusing the caller's
/// pre-opened storage for the local route when it belongs to the same
/// workspace (see [`execute_with_args_and_storage`]).
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_cli_with_storage(
    cli_args: &CliCloseArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    pre_opened: &mut Option<config::OpenStorageResult>,
) -> Result<()> {
    let args = CloseArgs::from(cli_args);
    execute_with_args_and_storage(&args, json, cli, ctx, pre_opened)
}

/// Result of a close operation for JSON output.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

/// Result of closing with suggest-next.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseWithSuggestResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    pub unblocked: Vec<UnblockedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

/// An issue that became unblocked after closing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnblockedIssue {
    pub id: String,
    pub title: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub closed_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
struct CloseExecution {
    closed: Vec<ClosedIssue>,
    skipped: Vec<SkippedIssue>,
    unblocked: Vec<UnblockedIssue>,
    ordered_outcomes: Vec<CloseOutcome>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

#[derive(Debug, Clone)]
enum CloseOutcome {
    Closed(ClosedIssue),
    Skipped(SkippedIssue),
}

fn build_close_json_payload(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
) -> Result<String> {
    let json = if args.suggest_next {
        // suggest_next is br-only, so always use the wrapped machine format.
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
            warnings: capacity_warnings,
        };
        serde_json::to_string_pretty(&result)?
    } else if skipped_issues.is_empty() && capacity_warnings.is_empty() {
        // Preserve bd-compatible array output for pure-success closes.
        serde_json::to_string_pretty(&closed_issues)?
    } else {
        // Once skips are present, a bare array loses machine-readable reasons.
        let result = CloseResult {
            closed: closed_issues,
            skipped: skipped_issues,
            warnings: capacity_warnings,
        };
        serde_json::to_string_pretty(&result)?
    };

    Ok(json)
}

fn render_close_json(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
) -> Result<()> {
    let json = build_close_json_payload(
        args,
        closed_issues,
        skipped_issues,
        unblocked_issues,
        capacity_warnings,
    )?;
    println!("{json}");
    Ok(())
}

fn emit_close_structured_output(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    ctx: &OutputContext,
) -> Result<()> {
    if args.suggest_next {
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
            warnings: capacity_warnings,
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else if ctx.is_json() {
            ctx.json_pretty(&result);
        } else {
            let json_ctx = OutputContext::from_flags(true, false, true);
            json_ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if skipped_issues.is_empty() && capacity_warnings.is_empty() {
        if ctx.is_toon() {
            ctx.toon(&closed_issues);
        } else if ctx.is_json() {
            ctx.json_pretty(&closed_issues);
        } else {
            render_close_json(
                args,
                closed_issues,
                skipped_issues,
                unblocked_issues,
                capacity_warnings,
            )?;
        }
        return Ok(());
    }

    let result = CloseResult {
        closed: closed_issues,
        skipped: skipped_issues,
        warnings: capacity_warnings,
    };
    if ctx.is_toon() {
        ctx.toon(&result);
    } else if ctx.is_json() {
        ctx.json_pretty(&result);
    } else {
        let json_ctx = OutputContext::from_flags(true, false, true);
        json_ctx.json_pretty(&result);
    }
    Ok(())
}

fn close_human_message(closed: &ClosedIssue) -> String {
    let id = sanitize_terminal_inline(&closed.id);
    let title = sanitize_terminal_inline(&closed.title);
    let mut message = format!("Closed {}: {}", id.as_ref(), title.as_ref());
    if let Some(reason) = &closed.close_reason {
        let reason = sanitize_terminal_inline(reason);
        message.push_str(" (");
        message.push_str(reason.as_ref());
        message.push(')');
    }
    message
}

fn skipped_human_message(skipped: &SkippedIssue) -> String {
    let id = sanitize_terminal_inline(&skipped.id);
    let reason = sanitize_terminal_inline(&skipped.reason);
    format!("Skipped {}: {}", id.as_ref(), reason.as_ref())
}

fn unblocked_human_line(issue: &UnblockedIssue) -> String {
    let id = sanitize_terminal_inline(&issue.id);
    let title = sanitize_terminal_inline(&issue.title);
    format!("  {}: {}", id.as_ref(), title.as_ref())
}

fn issue_input_text(input: &str) -> String {
    sanitize_terminal_inline(input).into_owned()
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

fn compute_batch_closable_ids(
    active_issue_ids: &HashSet<String>,
    internal_blockers_by_id: &HashMap<String, Vec<String>>,
    external_blockers_by_id: &HashMap<String, Vec<String>>,
) -> HashSet<String> {
    let mut closable: HashSet<String> = active_issue_ids
        .iter()
        .filter(|id| {
            external_blockers_by_id
                .get(*id)
                .is_none_or(std::vec::Vec::is_empty)
        })
        .cloned()
        .collect();

    loop {
        let to_remove: Vec<String> = closable
            .iter()
            .filter(|id| {
                internal_blockers_by_id
                    .get(*id)
                    .into_iter()
                    .flatten()
                    .any(|blocker_id| !closable.contains(blocker_id))
            })
            .cloned()
            .collect();

        if to_remove.is_empty() {
            break;
        }

        for id in to_remove {
            closable.remove(&id);
        }
    }

    closable
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
        ..CloseArgs::default()
    };

    execute_with_args(&args, json, cli, ctx)
}

/// Execute the close command with full arguments.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_with_args(
    args: &CloseArgs,
    use_json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    execute_with_args_and_storage(args, use_json, cli, ctx, &mut None)
}

/// Execute the close command with full arguments, reusing the caller's
/// pre-opened storage for the local route when it belongs to the same
/// workspace.
///
/// `main` opens storage once at startup (auto-import probe, auto-flush) and
/// keeps that connection alive across dispatch; opening a second one here was
/// a fixed cost on every close (bead naul5). The borrowed connection is handed
/// back through `pre_opened` after the write, so the startup auto-flush still
/// runs through the connection that performed it. A pre-opened context for a
/// different workspace is left untouched, and routed external batches always
/// open their own.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute_with_args_and_storage(
    args: &CloseArgs,
    use_json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    pre_opened: &mut Option<config::OpenStorageResult>,
) -> Result<()> {
    tracing::info!("Executing close command");
    let use_structured_output = use_json || ctx.is_json() || ctx.is_toon();

    // Up-front bypass argument-pair validation. Done before any storage IO so
    // a misuse of the bypass flag never silently slips past policy gates.
    validate_bypass_args(args)?;

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

    if args.suggest_next && target_inputs.len() > 1 {
        return Err(BeadsError::validation(
            "suggest-next",
            "--suggest-next only works with a single issue ID",
        ));
    }
    let routed_batches = config::routing::group_issue_inputs_by_route(&target_inputs, &beads_dir)?;

    let mut closed_issues = Vec::new();
    let mut skipped_issues = Vec::new();
    let mut unblocked_issues = Vec::new();
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

            let execution = execute_route(
                &batch_args,
                &batch_cli,
                ctx,
                &batch.beads_dir,
                batch.is_external,
            )?;
            let CloseExecution {
                unblocked,
                ordered_outcomes,
                capacity_warnings: route_warnings,
                ..
            } = execution;
            routed_outcomes.push((batch.issue_inputs, ordered_outcomes));
            unblocked_issues.extend(unblocked);
            capacity_warnings.extend(route_warnings);
        }

        let ordered_outcomes = reorder_routed_items_by_requested_inputs(
            &target_inputs,
            routed_outcomes,
            "close routing",
        )?;
        for outcome in ordered_outcomes {
            match outcome {
                CloseOutcome::Closed(issue) => closed_issues.push(issue),
                CloseOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let mut local_args = args.clone();
        local_args.ids = target_inputs;
        let reuse_pre_opened = pre_opened
            .as_ref()
            .is_some_and(|storage_ctx| same_workspace(&storage_ctx.paths.beads_dir, &beads_dir));
        let borrowed = if reuse_pre_opened {
            pre_opened.take()
        } else {
            None
        };
        let (execution, resources) =
            execute_route_with_storage(&local_args, cli, ctx, &beads_dir, false, borrowed)?;
        if reuse_pre_opened {
            *pre_opened = Some(resources.storage_ctx);
        }
        closed_issues = execution.closed;
        skipped_issues = execution.skipped;
        unblocked_issues = execution.unblocked;
        capacity_warnings = execution.capacity_warnings;
    }

    let closed_count = closed_issues.len();
    let skipped_count = skipped_issues.len();
    // Capture per-issue skip reasons BEFORE the vectors are moved into the
    // output emitters. When every issue is skipped, the terminal error must
    // carry the real reasons: a generic "all N skipped" used to imply
    // "already closed or not found" even when the skip was actually a
    // dependency block, sending operators down the wrong debugging path
    // (issue #380).
    let skip_summary = summarize_skip_reasons(&skipped_issues);

    if let Some(last_closed) = closed_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_closed.id);
    }

    if use_structured_output {
        emit_close_structured_output(
            args,
            closed_issues,
            skipped_issues,
            unblocked_issues,
            capacity_warnings,
            ctx,
        )?;
    } else if closed_issues.is_empty() && skipped_issues.is_empty() {
        ctx.info("No issues to close.");
    } else {
        for closed in &closed_issues {
            ctx.success(&close_human_message(closed));
        }
        for skipped in &skipped_issues {
            ctx.warning(&skipped_human_message(skipped));
        }
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
        if !unblocked_issues.is_empty() {
            ctx.newline();
            ctx.info(&format!("Unblocked {} issue(s):", unblocked_issues.len()));
            for issue in &unblocked_issues {
                ctx.print_line(&unblocked_human_line(issue));
            }
        }
    }

    // Exit status must carry the outcome for EVERY requested id, not just for
    // the all-or-nothing case. Gating this on `closed_count == 0` meant a batch
    // that closed one issue and refused another exited 0 with the refusal
    // visible only as a warning on stderr — so `br close <blocked> <closeable>`
    // reported success while the blocked issue was untouched, and stdout showed
    // an unqualified "✓ Closed". `docs/agent/ERRORS.md` tells callers to parse
    // stdout precisely when the exit code is 0, which made that transcript
    // authoritative. A no-op must not be able to read as success.
    if skipped_count > 0 {
        return Err(if closed_count == 0 {
            BeadsError::NothingToDo {
                reason: format!("all {skipped_count} issue(s) skipped — {skip_summary}"),
            }
        } else {
            BeadsError::CloseIncomplete {
                closed: closed_count,
                skipped: skipped_count,
                summary: skip_summary,
            }
        });
    }

    Ok(())
}

/// Render the per-issue skip reasons for the terminal `NothingToDo` error.
///
/// Lists up to five `id: reason` pairs (sanitized for
/// terminal safety) so the error names WHY each issue was skipped instead of
/// leaving the operator to guess (issue #380). Longer batches get a
/// `+N more` suffix; JSON/robot callers still receive the full skip list in
/// the structured payload.
fn summarize_skip_reasons(skipped: &[SkippedIssue]) -> String {
    const SKIP_SUMMARY_PREVIEW: usize = 5;
    let mut parts: Vec<String> = skipped
        .iter()
        .take(SKIP_SUMMARY_PREVIEW)
        .map(|s| {
            format!(
                "{}: {}",
                sanitize_terminal_inline(&s.id),
                sanitize_terminal_inline(&s.reason)
            )
        })
        .collect();
    if skipped.len() > SKIP_SUMMARY_PREVIEW {
        parts.push(format!("+{} more", skipped.len() - SKIP_SUMMARY_PREVIEW));
    }
    parts.join("; ")
}

fn execute_route(
    args: &CloseArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<CloseExecution> {
    execute_route_with_storage(args, cli, ctx, beads_dir, auto_flush_external, None)
        .map(|(execution, _resources)| execution)
}

/// Execute one route and hand its connection and lock back to the caller
/// instead of dropping them, so a borrowed startup connection can be returned
/// to `main` for the auto-flush.
#[allow(clippy::too_many_lines)]
fn execute_route_with_storage(
    args: &CloseArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    auto_flush_external: bool,
    pre_opened: Option<config::OpenStorageResult>,
) -> Result<(CloseExecution, RouteResources)> {
    let routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    // Reuse the routed authority for the storage open below; acquiring the
    // same database-family lock from a second descriptor in this process
    // would self-deadlock until the lock timeout (#409 routed cluster).
    let mut route_cli = cli.clone();
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let cli = &route_cli;
    let mut storage_ctx = if let Some(storage_ctx) = pre_opened {
        storage_ctx
    } else {
        config::open_storage_with_cli(beads_dir, cli)?
    };
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    // Closure-time policy gates (issue #274 Phase 1). Loading happens once per
    // route; if the file is absent the doc is the all-off default.
    let policy_doc = close_policy::load_for_beads_dir(beads_dir)?;
    // Active when close-policy gates are enabled (issue #274) OR the workflow
    // gate engine is configured (issue #312, layer 2). The latter must also
    // trigger per-issue gate evaluation at close time.
    let policy_active = policy_doc.close_policy.is_active()
        || policy_doc.workflow.gates_enforced()
        || !policy_doc.workflow.required_fields.is_empty();
    let attribution = resolve_attribution_for_close(args, &policy_doc);
    if args.bypass_policy && !policy_doc.allow_bypass {
        return Err(BeadsError::validation(
            "bypass-policy",
            ".beads/policy.yaml has allow_bypass: false; --bypass-policy is disabled",
        ));
    }

    let epic_counts = storage_ctx.storage.get_epic_counts()?;
    let blocked_before: Vec<String> = if args.suggest_next {
        storage_ctx
            .storage
            .get_blocked_issues()?
            .into_iter()
            .map(|(i, _)| i.id)
            .collect()
    } else {
        Vec::new()
    };

    let requested_ids: HashSet<String> = resolved_ids.iter().cloned().collect();
    let mut open_issues: HashMap<String, crate::model::Issue> = HashMap::new();
    let mut internal_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut external_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut closed_issues: Vec<ClosedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;
    let mut capacity_warnings = Vec::new();

    let mut atomic_updates = Vec::new();
    let mut planned_closes = Vec::new();
    for (outcome_index, id) in resolved_ids.iter().enumerate() {
        ordered_outcomes.push(None);
        tracing::info!(id = %id, "Closing issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status.is_terminal() {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if !args.force
            && let Some(&(total, closed)) = epic_counts.get(id)
            && closed < total
        {
            let label = if issue.issue_type == IssueType::Epic {
                "epic"
            } else {
                "parent issue"
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!(
                    "{label} has {}/{} open children (use --force to close anyway)",
                    total - closed,
                    total
                ),
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        // Supplementary guard: catch dot-notation children (e.g. `epic.1`,
        // `epic.2`) that exist in the issues table without a formal
        // parent-child dep row. These slip past `epic_counts` because
        // get_epic_counts only scans the dependencies table. Missing-dep
        // children occur with legacy-bd migrations, bulk JSONL imports,
        // and hand-edited JSONL. Without this check, closing the parent
        // silently orphans the open children.
        let requested_dot_children = if args.force {
            Vec::new()
        } else {
            let open_dot_children = storage_ctx.storage.get_open_dot_notation_children(id)?;
            let (requested_children, unrequested_children): (Vec<String>, Vec<String>) =
                open_dot_children
                    .into_iter()
                    .partition(|child_id| requested_ids.contains(child_id));
            if !unrequested_children.is_empty() {
                let label = if issue.issue_type == IssueType::Epic {
                    "epic"
                } else {
                    "parent issue"
                };
                let preview: Vec<String> = unrequested_children.iter().take(5).cloned().collect();
                let suffix = if unrequested_children.len() > preview.len() {
                    format!(", +{} more", unrequested_children.len() - preview.len())
                } else {
                    String::new()
                };
                let skipped = SkippedIssue {
                    id: id.clone(),
                    reason: format!(
                        "{label} has {} open dot-notation child issue(s): {}{} (use --force to close anyway)",
                        unrequested_children.len(),
                        preview.join(", "),
                        suffix
                    ),
                };
                ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
                skipped_issues.push(skipped);
                continue;
            }
            requested_children
        };

        if args.force {
            open_issues.insert(id.clone(), issue);
            continue;
        }

        // Use *close* blockers, not generic blockers: a `parent-child` edge is
        // hierarchy, not a prerequisite from the parent to the child, so a
        // finished child must be closable even while its parent epic is itself
        // blocked or open (#355). `get_close_blockers` strips the propagated
        // `:parent-blocked` markers while retaining real prerequisite edges on
        // the child and the `:child-open` close-ordering rollup.
        let close_blockers_result = storage_ctx.storage.get_close_blockers(id);
        let mut blocker_ids = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            close_blockers_result,
        )?;
        blocker_ids.extend(requested_dot_children);
        blocker_ids.sort();
        blocker_ids.dedup();
        let (internal_blockers, external_blockers): (Vec<String>, Vec<String>) = blocker_ids
            .into_iter()
            .partition(|blocker_id| requested_ids.contains(blocker_id));
        internal_blockers_by_id.insert(id.clone(), internal_blockers);
        external_blockers_by_id.insert(id.clone(), external_blockers);
        open_issues.insert(id.clone(), issue);
    }

    let active_issue_ids: HashSet<String> = open_issues.keys().cloned().collect();
    let batch_closable_ids = if args.force {
        active_issue_ids
    } else {
        compute_batch_closable_ids(
            &active_issue_ids,
            &internal_blockers_by_id,
            &external_blockers_by_id,
        )
    };

    let mut policy_evaluations_by_id: HashMap<String, EvaluatedGates> = HashMap::new();
    if policy_active {
        for id in &resolved_ids {
            let Some(issue) = open_issues.get(id) else {
                continue;
            };
            if !args.force && !batch_closable_ids.contains(id) {
                continue;
            }

            let evaluated_gates = evaluate_close_policy(
                &policy_doc.close_policy,
                &policy_doc.workflow,
                &storage_ctx.storage,
                id,
                issue,
                args,
                &actor,
            )?;
            if !evaluated_gates.violations.is_empty() && !args.bypass_policy {
                let summary = summarize_violations(&evaluated_gates.violations);
                return Err(BeadsError::PolicyViolation {
                    issue_id: id.clone(),
                    summary,
                    violations: evaluated_gates.violations,
                });
            }
            policy_evaluations_by_id.insert(id.clone(), evaluated_gates);
        }
    }

    for (outcome_index, id) in resolved_ids.iter().enumerate() {
        let Some(issue) = open_issues.get(id) else {
            continue;
        };

        if !args.force && !batch_closable_ids.contains(id) {
            let mut blocker_ids = external_blockers_by_id.get(id).cloned().unwrap_or_default();
            if let Some(internal_blockers) = internal_blockers_by_id.get(id) {
                blocker_ids.extend(
                    internal_blockers
                        .iter()
                        .filter(|blocker_id| !batch_closable_ids.contains(*blocker_id))
                        .cloned(),
                );
            }
            blocker_ids.sort();
            blocker_ids.dedup();
            tracing::debug!(blocked_by = ?blocker_ids, "Issue remains blocked in batch close");
            // Name the open blockers AND the way out. Without the explicit
            // remediation this skip used to surface as a bare "all N issue(s)
            // skipped" error whose hint claimed the issue was "already closed
            // or not found" — flatly wrong for a dependency block (#380).
            let reason = if blocker_ids.is_empty() {
                "blocked by dependencies — close the open blocker(s) first, or use --force to close anyway".to_string()
            } else {
                format!(
                    "blocked by: {} — close the open blocker(s) first, or use --force to close anyway",
                    blocker_ids.join(", ")
                )
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason,
            };
            ordered_outcomes[outcome_index] = Some(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        let gates_fired = if let Some(evaluated_gates) = policy_evaluations_by_id.get(id) {
            if !evaluated_gates.violations.is_empty() && args.bypass_policy {
                emit_bypass_warning(ctx, id, &evaluated_gates.violations);
            }
            evaluated_gates
                .violations
                .iter()
                .map(|v| v.gate.clone())
                .collect::<Vec<String>>()
        } else {
            Vec::new()
        };

        let now = Utc::now();
        let close_reason = args.reason.clone().unwrap_or_else(|| "done".to_string());
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: Some(Some(close_reason.clone())),
            closed_by_session: args.session.clone().map(Some),
            transition_comment: args.transition_comment.clone(),
            workflow_policy_bypass_reason: if args.bypass_policy {
                args.bypass_reason.clone()
            } else {
                None
            },
            ..Default::default()
        };

        atomic_updates.push((id.clone(), update));
        planned_closes.push((
            outcome_index,
            id.clone(),
            issue.clone(),
            gates_fired,
            now,
            close_reason,
        ));
    }

    if !atomic_updates.is_empty() {
        storage_ctx
            .storage
            .set_pending_event_attribution(EventAttribution::new(
                attribution.agent_name.as_deref(),
                attribution.harness.as_deref(),
                attribution.model.as_deref(),
                super::session_attribution_from_env().as_deref(),
            ));
        let update_result = update_issues_atomically_with_recovery(
            &mut storage_ctx,
            true,
            "close",
            &atomic_updates,
            &actor,
        );
        preserve_blocked_cache_on_error(&mut storage_ctx.storage, false, "close", update_result)?;
        capacity_warnings = storage_ctx.storage.take_capacity_warnings();
        cache_dirty = true;
    }

    for (outcome_index, id, issue, gates_fired, now, close_reason) in planned_closes {
        tracing::info!(id = %id, reason = ?args.reason, "Issue closed");

        if policy_active {
            // Best-effort persistence of attribution + bypass auditing. Failure
            // to record metadata never undoes a successful close: the close
            // already committed, and burning down a successful close because of
            // an optional auxiliary table would be a regression for users whose
            // schema could not migrate. We log and move on.
            let bypass_reason = if args.bypass_policy {
                args.bypass_reason.as_deref()
            } else {
                None
            };
            let metadata_result = storage_ctx.storage.record_close_metadata(
                &id,
                &attribution,
                args.bypass_policy && !gates_fired.is_empty(),
                bypass_reason,
                &gates_fired,
            );
            if let Err(error) = metadata_result {
                tracing::warn!(
                    issue_id = %id,
                    error = %error,
                    "failed to record closure-time policy metadata; close already committed"
                );
            }
        }

        let closed = ClosedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "closed".to_string(),
            closed_at: now.to_rfc3339(),
            close_reason: Some(close_reason),
        };
        ordered_outcomes[outcome_index] = Some(CloseOutcome::Closed(closed.clone()));
        closed_issues.push(closed);
    }

    let ordered_outcomes = ordered_outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| BeadsError::internal("close batch outcome was not populated"))
        })
        .collect::<Result<Vec<_>>>()?;

    if cache_dirty
        && storage_ctx
            .storage
            .blocked_cache_marked_stale()
            .unwrap_or(true)
    {
        tracing::info!(
            "Rebuilding blocked cache after closing {} issues",
            closed_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "close")?;
    }

    let unblocked_issues: Vec<UnblockedIssue> = if args.suggest_next && !closed_issues.is_empty() {
        let blocked_after_result = storage_ctx.storage.get_blocked_issues();
        let blocked_after = match preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            blocked_after_result,
        ) {
            Ok(blocked_after) => Some(
                blocked_after
                    .into_iter()
                    .map(|(issue, _)| issue.id)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Skipping suggest-next calculation after committed close because blocked-cache lookup failed"
                );
                None
            }
        };

        let Some(blocked_after) = blocked_after else {
            storage_ctx.flush_no_db_if_dirty()?;
            return Ok((
                CloseExecution {
                    closed: closed_issues,
                    skipped: skipped_issues,
                    unblocked: Vec::new(),
                    ordered_outcomes,
                    capacity_warnings,
                },
                RouteResources {
                    storage_ctx,
                    _routed_write_lock: routed_write_lock,
                },
            ));
        };

        let newly_unblocked: Vec<String> = blocked_before
            .into_iter()
            .filter(|id| !blocked_after.contains(id))
            .collect();

        tracing::debug!(unblocked = ?newly_unblocked, "Issues unblocked by close");

        let mut unblocked = Vec::new();
        for uid in newly_unblocked {
            let issue_result = storage_ctx.storage.get_issue(&uid);
            match preserve_blocked_cache_on_error(
                &mut storage_ctx.storage,
                cache_dirty,
                "close",
                issue_result,
            ) {
                Ok(Some(issue)) if issue.status.is_active() => {
                    unblocked.push(UnblockedIssue {
                        id: issue.id,
                        title: issue.title,
                        priority: issue.priority.0,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        issue_id = %uid,
                        error = %error,
                        "Skipping suggest-next candidate after committed close because issue lookup failed"
                    );
                }
            }
        }
        unblocked
    } else {
        Vec::new()
    };

    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok((
        CloseExecution {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
            ordered_outcomes,
            capacity_warnings,
        },
        RouteResources {
            storage_ctx,
            _routed_write_lock: routed_write_lock,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::error::StructuredError;
    use crate::model::{DependencyType, Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use std::env;
    use std::path::PathBuf;

    use tempfile::TempDir;

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &std::path::Path) -> Self {
            let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
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
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            ..Issue::default()
        }
    }

    fn make_issue_with_status(id: &str, title: &str, status: Status) -> Issue {
        Issue {
            status,
            ..make_issue(id, title)
        }
    }

    // =========================================================================
    // CloseArgs tests
    // =========================================================================

    // === Startup connection reuse (beads_rust-naul5) ===

    fn reuse_workspace() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create beads dir");
        let mut storage_ctx =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        storage_ctx
            .storage
            .create_issue(&make_issue("bd-cl", "Close target"), "tester")
            .expect("create issue");
        (temp, beads_dir)
    }

    #[test]
    fn execute_with_args_and_storage_reuses_the_pre_opened_connection_and_hands_it_back() {
        let (_temp, beads_dir) = reuse_workspace();
        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };
        let mut pre_opened =
            Some(config::open_storage_with_cli(&beads_dir, &cli).expect("pre-open storage"));
        let args = CloseArgs {
            ids: vec!["bd-cl".to_string()],
            ..CloseArgs::default()
        };

        execute_with_args_and_storage(
            &args,
            true,
            &cli,
            &OutputContext::from_flags(true, false, true),
            &mut pre_opened,
        )
        .expect("close through the pre-opened storage");

        let storage_ctx = pre_opened.expect("the borrowed connection is handed back");
        let issue = storage_ctx
            .storage
            .get_issue("bd-cl")
            .expect("read issue")
            .expect("issue exists");
        assert_eq!(
            issue.status,
            Status::Closed,
            "the close is visible through the connection that was handed back"
        );
    }

    #[test]
    fn execute_with_args_and_storage_leaves_a_foreign_pre_opened_connection_alone() {
        let (_temp_a, beads_dir_a) = reuse_workspace();
        let (_temp_b, beads_dir_b) = reuse_workspace();
        let cli_b = CliOverrides {
            db: Some(beads_dir_b.join("beads.db")),
            ..CliOverrides::default()
        };
        let mut pre_opened = Some(
            config::open_storage_with_cli(&beads_dir_a, &CliOverrides::default())
                .expect("pre-open workspace A"),
        );
        let args = CloseArgs {
            ids: vec!["bd-cl".to_string()],
            ..CloseArgs::default()
        };

        execute_with_args_and_storage(
            &args,
            true,
            &cli_b,
            &OutputContext::from_flags(true, false, true),
            &mut pre_opened,
        )
        .expect("close in workspace B");

        let kept = pre_opened.expect("a foreign connection stays with the caller");
        assert!(
            crate::cli::commands::same_workspace(&kept.paths.beads_dir, &beads_dir_a),
            "the caller's connection still belongs to workspace A"
        );
        assert_eq!(
            kept.storage
                .get_issue("bd-cl")
                .expect("read workspace A")
                .expect("A's issue exists")
                .status,
            Status::Open,
            "workspace A was not written"
        );
        let reopened_b = config::open_storage_with_cli(&beads_dir_b, &CliOverrides::default())
            .expect("reopen workspace B");
        assert_eq!(
            reopened_b
                .storage
                .get_issue("bd-cl")
                .expect("read workspace B")
                .expect("B's issue exists")
                .status,
            Status::Closed,
            "workspace B received the close through its own connection"
        );
    }

    #[test]
    fn test_close_args_default() {
        let args = CloseArgs::default();
        assert!(args.ids.is_empty());
        assert!(args.reason.is_none());
        assert!(args.transition_comment.is_none());
        assert!(!args.force);
        assert!(args.session.is_none());
        assert!(!args.suggest_next);
    }

    #[test]
    fn test_close_args_with_all_fields() {
        let args = CloseArgs {
            ids: vec!["bd-abc".to_string(), "bd-xyz".to_string()],
            reason: Some("Fixed in PR #123".to_string()),
            transition_comment: Some("Verified in staging".to_string()),
            force: true,
            session: Some("session-456".to_string()),
            suggest_next: true,
            agent_name: Some("agent-1".to_string()),
            harness: Some("codex-cli".to_string()),
            model: Some("gpt-5".to_string()),
            bypass_policy: true,
            bypass_reason: Some("Manual override approved".to_string()),
        };
        assert_eq!(args.ids.len(), 2);
        assert_eq!(args.ids[0], "bd-abc");
        assert_eq!(args.reason.as_deref(), Some("Fixed in PR #123"));
        assert_eq!(
            args.transition_comment.as_deref(),
            Some("Verified in staging")
        );
        assert!(args.force);
        assert_eq!(args.session.as_deref(), Some("session-456"));
        assert!(args.suggest_next);
        assert_eq!(args.agent_name.as_deref(), Some("agent-1"));
        assert_eq!(args.harness.as_deref(), Some("codex-cli"));
        assert_eq!(args.model.as_deref(), Some("gpt-5"));
        assert!(args.bypass_policy);
        assert_eq!(
            args.bypass_reason.as_deref(),
            Some("Manual override approved")
        );
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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
            warnings: vec![],
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

    #[test]
    fn close_human_messages_sanitize_terminal_controls() {
        let closed = ClosedIssue {
            id: "bd-close\x1b[2J".to_string(),
            title: "bad\rtitle\x08".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-12-31T23:59:59Z".to_string(),
            close_reason: Some("done\nnext\x07\u{9b}".to_string()),
        };
        let skipped = SkippedIssue {
            id: "bd-skip\x1b[2J".to_string(),
            reason: "blocked\rby\nterminal\x07".to_string(),
        };
        let unblocked = UnblockedIssue {
            id: "bd-unblock\x1b[2J".to_string(),
            title: "ready\nlater\x08".to_string(),
            priority: 1,
        };

        let close_message = close_human_message(&closed);
        let skipped_message = skipped_human_message(&skipped);
        let unblocked_line = unblocked_human_line(&unblocked);

        for text in [&close_message, &skipped_message, &unblocked_line] {
            assert!(!text.chars().any(char::is_control));
            assert!(text.contains("\\u{1b}[2J"));
        }
        assert!(close_message.contains("\\r"));
        assert!(close_message.contains("\\u{8}"));
        assert!(close_message.contains("\\n"));
        assert!(close_message.contains("\\u{7}"));
        assert!(close_message.contains("\\u{9b}"));
        assert!(skipped_message.contains("\\r"));
        assert!(skipped_message.contains("\\n"));
        assert!(skipped_message.contains("\\u{7}"));
        assert!(unblocked_line.contains("\\n"));
        assert!(unblocked_line.contains("\\u{8}"));
    }

    #[test]
    fn reorder_routed_items_sanitizes_missing_input_error() {
        let requested = vec!["bd-close\x1b[2J\nbad".to_string(), "bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-ok".to_string()], vec!["ok"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "close routing")
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
        let routed_items = vec![(vec!["bd-close\x1b[2J\nbad".to_string()], vec!["bad"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "close routing")
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
            warnings: vec![],
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 2);
    }

    #[test]
    fn test_render_close_json_preserves_bare_array_for_pure_success() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![],
            vec![],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
    }

    #[test]
    fn test_close_result_shape_with_skipped_is_wrapped() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![SkippedIssue {
                id: "bd-2".to_string(),
                reason: "blocked by: bd-3".to_string(),
            }],
            vec![],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["closed"][0]["id"], "bd-1");
        assert_eq!(parsed["skipped"][0]["id"], "bd-2");
    }

    #[test]
    fn test_close_args_clone() {
        let args = CloseArgs {
            ids: vec!["bd-clone".to_string()],
            reason: Some("Clone test".to_string()),
            transition_comment: Some("Fresh transition evidence".to_string()),
            force: true,
            session: Some("sess".to_string()),
            suggest_next: true,
            agent_name: Some("agent-clone".to_string()),
            harness: Some("harness-clone".to_string()),
            model: Some("model-clone".to_string()),
            bypass_policy: true,
            bypass_reason: Some("Clone bypass reason".to_string()),
        };
        let cloned = args.clone();
        assert_eq!(cloned.ids, args.ids);
        assert_eq!(cloned.reason, args.reason);
        assert_eq!(cloned.transition_comment, args.transition_comment);
        assert_eq!(cloned.force, args.force);
        assert_eq!(cloned.session, args.session);
        assert_eq!(cloned.suggest_next, args.suggest_next);
        assert_eq!(cloned.agent_name, args.agent_name);
        assert_eq!(cloned.harness, args.harness);
        assert_eq!(cloned.model, args.model);
        assert_eq!(cloned.bypass_policy, args.bypass_policy);
        assert_eq!(cloned.bypass_reason, args.bypass_reason);
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

    #[test]
    fn execute_with_args_closes_requested_blocker_chain_in_one_batch() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-blocker", "Batch blocker"), "tester")
            .expect("create blocker");
        storage
            .create_issue(&make_issue("bd-blocked", "Batch blocked"), "tester")
            .expect("create blocked");
        storage
            .add_dependency(
                "bd-blocked",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-blocked".to_string(), "bd-blocker".to_string()],
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx).expect("close batch");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let blocker = storage
            .get_issue("bd-blocker")
            .expect("get blocker")
            .expect("blocker exists");
        let blocked_issue = storage
            .get_issue("bd-blocked")
            .expect("get blocked")
            .expect("blocked exists");

        assert_eq!(blocker.status, Status::Closed);
        assert_eq!(blocked_issue.status, Status::Closed);
    }

    #[test]
    fn execute_route_preserves_request_order_for_mixed_close_outcomes() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-close-first", "First"), "tester")
            .expect("create first");
        let mut skipped = make_issue_with_status("bd-close-skip", "Skip", Status::Closed);
        skipped.closed_at = Some(Utc::now());
        storage
            .create_issue(&skipped, "tester")
            .expect("create skipped");
        storage
            .create_issue(&make_issue("bd-close-last", "Last"), "tester")
            .expect("create last");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec![
                "bd-close-first".to_string(),
                "bd-close-skip".to_string(),
                "bd-close-last".to_string(),
            ],
            ..CloseArgs::default()
        };
        let execution = execute_route(&args, &CliOverrides::default(), &ctx, &beads_dir, false)
            .expect("mixed close batch");

        assert!(matches!(
            &execution.ordered_outcomes[0],
            CloseOutcome::Closed(issue) if issue.id == "bd-close-first"
        ));
        assert!(matches!(
            &execution.ordered_outcomes[1],
            CloseOutcome::Skipped(issue) if issue.id == "bd-close-skip"
        ));
        assert!(matches!(
            &execution.ordered_outcomes[2],
            CloseOutcome::Closed(issue) if issue.id == "bd-close-last"
        ));
    }

    #[test]
    fn execute_with_args_closes_requested_dot_notation_child_with_parent() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-parent", "Legacy parent"), "tester")
            .expect("create parent");
        storage
            .create_issue(&make_issue("bd-parent.1", "Legacy child"), "tester")
            .expect("create dot child");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-parent".to_string(), "bd-parent.1".to_string()],
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close parent and dot child in one batch");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        let child = storage
            .get_issue("bd-parent.1")
            .expect("get child")
            .expect("child exists");

        assert_eq!(parent.status, Status::Closed);
        assert_eq!(child.status, Status::Closed);
    }

    #[test]
    fn execute_with_args_keeps_parent_blocked_by_unrequested_dot_notation_child() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-parent", "Legacy parent"), "tester")
            .expect("create parent");
        storage
            .create_issue(&make_issue("bd-parent.1", "Legacy child"), "tester")
            .expect("create dot child");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-parent".to_string()],
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, true, &CliOverrides::default(), &ctx)
            .expect_err("parent-only close should remain blocked by dot child");
        assert!(matches!(err, BeadsError::NothingToDo { .. }));

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        let child = storage
            .get_issue("bd-parent.1")
            .expect("get child")
            .expect("child exists");

        assert_eq!(parent.status, Status::Open);
        assert_eq!(child.status, Status::Open);
    }

    #[test]
    fn execute_with_args_closes_child_even_when_parent_epic_is_blocked() {
        // Regression for #355: a finished child must be closable even while its
        // parent epic is itself blocked by an unrelated prerequisite. The
        // `parent-child` edge is hierarchy, not a prerequisite from parent to
        // child, so the propagated `:parent-blocked` marker must not gate the
        // child's own closure.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        // A deferred prerequisite that blocks the parent epic.
        storage
            .create_issue(
                &make_issue_with_status("bd-blocker", "Deferred blocker", Status::Deferred),
                "tester",
            )
            .expect("create blocker");
        // The parent epic, blocked by the deferred prerequisite.
        let mut parent = make_issue("bd-parent", "Parent epic");
        parent.issue_type = IssueType::Epic;
        storage
            .create_issue(&parent, "tester")
            .expect("create parent");
        storage
            .add_dependency(
                "bd-parent",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add blocks dependency");
        // A child of the parent epic, reviewed-complete and ready to close.
        storage
            .create_issue(&make_issue("bd-child", "Child task"), "tester")
            .expect("create child");
        storage
            .add_dependency(
                "bd-child",
                "bd-parent",
                DependencyType::ParentChild.as_str(),
                "tester",
            )
            .expect("add parent-child dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        // The child IS propagated `parent-blocked` in the readiness graph...
        assert!(
            storage
                .get_blockers("bd-child")
                .expect("get blockers")
                .contains(&"bd-parent".to_string()),
            "child should inherit a parent-blocked readiness marker"
        );
        // ...but it must NOT be a *close* blocker.
        assert!(
            storage
                .get_close_blockers("bd-child")
                .expect("get close blockers")
                .is_empty(),
            "a blocked parent must not gate the child's closure"
        );
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-child".to_string()],
            reason: Some("Child done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("child close should succeed despite blocked parent");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let child = storage
            .get_issue("bd-child")
            .expect("get child")
            .expect("child exists");
        let parent = storage
            .get_issue("bd-parent")
            .expect("get parent")
            .expect("parent exists");
        assert_eq!(child.status, Status::Closed, "child should be closed");
        assert_eq!(
            parent.status,
            Status::Open,
            "parent epic should remain open/blocked"
        );
    }

    #[test]
    fn execute_with_args_returns_nothing_to_do_when_all_requested_issues_are_skipped() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let mut issue = make_issue("bd-closed", "Already closed");
        issue.status = Status::Closed;
        issue.closed_at = Some(Utc::now());
        storage
            .create_issue(&issue, "tester")
            .expect("create closed issue");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-closed".to_string()],
            ..CloseArgs::default()
        };

        let err = execute_with_args(&args, true, &CliOverrides::default(), &ctx)
            .expect_err("all-skipped close should fail");
        assert!(matches!(err, BeadsError::NothingToDo { .. }));
    }

    #[test]
    fn execute_with_args_reports_partial_batch_as_error_not_success() {
        // A batch that closes one issue and refuses another used to return
        // `Ok(())`, because the terminal error was gated on `closed_count == 0`.
        // The refusal was then visible ONLY as a warning on stderr, while stdout
        // carried an unqualified "✓ Closed" and `$?` was 0 — and
        // `docs/agent/ERRORS.md` instructs callers to parse stdout precisely
        // when the exit code is 0. Any caller branching on exit status read the
        // untouched issue as closed.
        //
        // This is the direction that fires: under the old predicate this test
        // fails at `expect_err`. The all-succeed case is covered by
        // `execute_with_args_closes_requested_blocker_chain_in_one_batch`, which
        // still expects `Ok(())` — so the new predicate is proven in both
        // directions.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-blocker", "Unrequested blocker"), "tester")
            .expect("create blocker");
        storage
            .create_issue(
                &make_issue("bd-blocked", "Blocked by unrequested"),
                "tester",
            )
            .expect("create blocked");
        storage
            .create_issue(&make_issue("bd-free", "Independently closeable"), "tester")
            .expect("create free");
        storage
            .add_dependency(
                "bd-blocked",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-blocked".to_string(), "bd-free".to_string()],
            ..CloseArgs::default()
        };

        let err = execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect_err("a partially applied batch must not report success");
        assert!(
            matches!(err, BeadsError::CloseIncomplete { .. }),
            "a partial batch must surface as CloseIncomplete, got: {err:?}"
        );
        // Destructure without a diverging arm: the fallback carries values that
        // fail every assertion below, so a wrong variant is still loud.
        let (closed, skipped, summary) = match &err {
            BeadsError::CloseIncomplete {
                closed,
                skipped,
                summary,
            } => (*closed, *skipped, summary.clone()),
            other => (0, 0, format!("wrong variant: {other:?}")),
        };
        assert_eq!(closed, 1, "exactly one issue should have closed");
        assert_eq!(skipped, 1, "exactly one issue should have been skipped");
        assert!(
            summary.contains("bd-blocked") && summary.contains("blocked by"),
            "summary must name the refused issue and the real reason, got: {summary}"
        );

        // The exit status now agrees with the record, which is the whole point.
        assert_ne!(
            StructuredError::from_error(&err).code.exit_code(),
            0,
            "a partial close must not exit 0"
        );

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        assert_eq!(
            storage
                .get_issue("bd-free")
                .expect("get free")
                .expect("free exists")
                .status,
            Status::Closed,
            "the closeable issue should still have been closed"
        );
        assert_eq!(
            storage
                .get_issue("bd-blocked")
                .expect("get blocked")
                .expect("blocked exists")
                .status,
            Status::Open,
            "the blocked issue must remain open — the error is what reports that"
        );
    }

    #[test]
    fn execute_with_args_records_clean_policy_close_metadata() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-policy", "Policy governed"), "tester")
            .expect("create policy issue");
        drop(storage);

        std::fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            "close_policy:\n  require_close_reason:\n    enabled: true\n    min_length: 4\n",
        )
        .expect("write policy");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-policy".to_string()],
            reason: Some("done cleanly".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx).expect("close issue");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let metadata = storage
            .get_close_metadata("bd-policy")
            .expect("read close metadata")
            .expect("active policy should record metadata even when no gate fires");
        assert!(!metadata.bypassed_policy);
        assert!(metadata.bypass_reason.is_none());
        assert!(metadata.policy_gates_fired.is_empty());
        assert!(metadata.closed_by_agent_name.is_none());
        assert!(metadata.closed_by_harness.is_none());
        assert!(metadata.closed_by_model.is_none());
    }

    #[test]
    fn execute_with_args_policy_violation_in_batch_does_not_close_earlier_issue() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-clean", "Clean policy issue"), "tester")
            .expect("create clean issue");
        let mut failing_issue = make_issue("bd-policy-fail", "Policy failing issue");
        failing_issue.acceptance_criteria = Some("- [ ] Finish remaining work\n".to_string());
        storage
            .create_issue(&failing_issue, "tester")
            .expect("create failing issue");
        drop(storage);

        std::fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            "close_policy:\n  require_acceptance_criteria_satisfied:\n    enabled: true\n",
        )
        .expect("write policy");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-clean".to_string(), "bd-policy-fail".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect_err("policy violation should abort the batch before mutation");
        assert!(
            matches!(err, BeadsError::PolicyViolation { .. }),
            "unexpected error: {err:?}"
        );

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let clean = storage
            .get_issue("bd-clean")
            .expect("read clean issue")
            .expect("clean issue exists");
        let failing = storage
            .get_issue("bd-policy-fail")
            .expect("read failing issue")
            .expect("failing issue exists");
        assert_eq!(clean.status, Status::Open);
        assert_eq!(failing.status, Status::Open);
        assert!(
            storage
                .get_close_metadata("bd-clean")
                .expect("read clean metadata")
                .is_none()
        );
    }

    #[test]
    fn execute_with_args_leaves_no_policy_close_metadata_invisible() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-no-policy", "No policy"), "tester")
            .expect("create no-policy issue");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-no-policy".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx).expect("close issue");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        assert!(
            storage
                .get_close_metadata("bd-no-policy")
                .expect("read close metadata")
                .is_none(),
            "repos without an active policy should retain the no-observable-change invariant"
        );
    }

    /// GitHub #399: `br close` must honor `workflow.transitions`. A policy
    /// whose map has no `open -> closed` edge has to refuse the close and
    /// leave the issue open; widening the map with the `any` wildcard has to
    /// let the same close through.
    #[test]
    fn execute_with_args_enforces_workflow_transitions_on_close() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-wf", "Workflow governed"), "tester")
            .expect("create workflow issue");
        drop(storage);

        let policy_path = beads_dir.join(close_policy::POLICY_FILE_NAME);
        std::fs::write(
            &policy_path,
            "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n  \
             transitions:\n    open: [in_progress]\n    in_progress: [closed]\n",
        )
        .expect("write workflow policy");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-wf".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect_err("open -> closed is not in the transitions map");
        let message = err.to_string();
        assert!(
            message.contains("workflow.transitions") && message.contains("'open'"),
            "error should name the rejected transition: {message}"
        );

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        assert_eq!(
            storage
                .get_issue("bd-wf")
                .expect("read issue")
                .expect("issue exists")
                .status,
            Status::Open,
            "a refused close must leave the issue open"
        );
        drop(storage);

        // Widening the map with the `any` wildcard makes the same close legal.
        std::fs::write(
            &policy_path,
            "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n  \
             transitions:\n    open: [in_progress]\n    any: [closed]\n",
        )
        .expect("rewrite workflow policy");
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("wildcard `any: [closed]` permits the close");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        assert_eq!(
            storage
                .get_issue("bd-wf")
                .expect("read issue")
                .expect("issue exists")
                .status,
            Status::Closed
        );
    }

    // =========================================================================
    // forbid_close_with_deferred_dependents gate (beads_rust#303)
    // =========================================================================

    const DEFERRED_DEPENDENTS_POLICY: &str =
        "close_policy:\n  forbid_close_with_deferred_dependents:\n    enabled: true\n";

    /// Build a prereq bead `bd-prereq` and a dependent bead `bd-dep` with a
    /// `blocks` edge from the prereq (so `bd-dep` depends on `bd-prereq`),
    /// where the dependent has the supplied status.
    fn setup_prereq_and_dependent(
        beads_dir: &std::path::Path,
        db_path: &std::path::Path,
        dependent_status: Status,
    ) {
        let mut storage = SqliteStorage::open(db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-prereq", "Prerequisite"), "tester")
            .expect("create prereq");
        storage
            .create_issue(
                &make_issue_with_status("bd-dep", "Dependent", dependent_status),
                "tester",
            )
            .expect("create dependent");
        // Row (issue_id=bd-dep, depends_on_id=bd-prereq, type=blocks):
        // "bd-prereq blocks bd-dep" / "bd-dep depends on bd-prereq".
        storage
            .add_dependency(
                "bd-dep",
                "bd-prereq",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        drop(storage);
        // No policy.yaml written by default — callers add it when needed.
        let _ = beads_dir;
    }

    #[test]
    fn deferred_dependents_gate_off_allows_close_by_default() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        // Deferred dependent present, but NO policy.yaml => gate is off.
        setup_prereq_and_dependent(&beads_dir, &db_path, Status::Deferred);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-prereq".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close should succeed when gate is off (backwards compatible)");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let prereq = storage
            .get_issue("bd-prereq")
            .expect("get prereq")
            .expect("prereq exists");
        assert_eq!(prereq.status, Status::Closed);
    }

    #[test]
    fn deferred_dependents_gate_on_rejects_close_and_names_ids() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        setup_prereq_and_dependent(&beads_dir, &db_path, Status::Deferred);
        std::fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            DEFERRED_DEPENDENTS_POLICY,
        )
        .expect("write policy");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-prereq".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect_err("close should be rejected by the deferred-dependents gate");

        match err {
            BeadsError::PolicyViolation {
                issue_id,
                summary,
                violations,
            } => {
                assert_eq!(issue_id, "bd-prereq");
                assert!(summary.contains("bd-dep"), "summary: {summary}");
                assert!(
                    violations.iter().any(|v| v.gate
                        == close_policy::GATE_FORBID_CLOSE_WITH_DEFERRED_DEPENDENTS),
                    "expected deferred-dependents gate to fire: {violations:?}"
                );
            }
            other => panic!("expected PolicyViolation, got {other:?}"),
        }

        // The prereq must NOT have been closed.
        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let prereq = storage
            .get_issue("bd-prereq")
            .expect("get prereq")
            .expect("prereq exists");
        assert_ne!(prereq.status, Status::Closed);
    }

    #[test]
    fn deferred_dependents_gate_on_allows_when_dependent_not_deferred() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        // Dependent is open, not deferred => gate is satisfied.
        setup_prereq_and_dependent(&beads_dir, &db_path, Status::Open);
        std::fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            DEFERRED_DEPENDENTS_POLICY,
        )
        .expect("write policy");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-prereq".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close should succeed when no dependent is deferred");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let prereq = storage
            .get_issue("bd-prereq")
            .expect("get prereq")
            .expect("prereq exists");
        assert_eq!(prereq.status, Status::Closed);
    }

    #[test]
    fn deferred_dependents_gate_on_allows_after_reopening_dependent() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        setup_prereq_and_dependent(&beads_dir, &db_path, Status::Deferred);
        std::fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            DEFERRED_DEPENDENTS_POLICY,
        )
        .expect("write policy");

        // First attempt is rejected.
        {
            let _guard = DirGuard::new(temp.path());
            let args = CloseArgs {
                ids: vec!["bd-prereq".to_string()],
                reason: Some("done".to_string()),
                ..CloseArgs::default()
            };
            execute_with_args(&args, false, &CliOverrides::default(), &ctx)
                .expect_err("close should be rejected while dependent is deferred");
        }

        // Reopen the deferred dependent (`br update bd-dep --status=open`).
        {
            let mut storage = SqliteStorage::open(&db_path).expect("storage");
            let update = IssueUpdate {
                status: Some(Status::Open),
                ..Default::default()
            };
            storage
                .update_issue("bd-dep", &update, "tester")
                .expect("reopen dependent");
        }

        // Second attempt now succeeds.
        {
            let _guard = DirGuard::new(temp.path());
            let args = CloseArgs {
                ids: vec!["bd-prereq".to_string()],
                reason: Some("done".to_string()),
                ..CloseArgs::default()
            };
            execute_with_args(&args, false, &CliOverrides::default(), &ctx)
                .expect("close should succeed after the dependent is reopened");
        }

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let prereq = storage
            .get_issue("bd-prereq")
            .expect("get prereq")
            .expect("prereq exists");
        assert_eq!(prereq.status, Status::Closed);
    }

    // =========================================================================
    // Workflow gate enforcement at close (issue #312, layer 2 / beads_rust#319)
    // =========================================================================

    const GATE_POLICY_YAML: &str = r#"workflow:
  strict: true
  gates:
    "in_review -> closed":
      require_all:
        - ci_green
"#;

    fn setup_gate_repo(temp: &TempDir, status: Status) -> std::path::PathBuf {
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");
        let beads_dir = temp.path().join(".beads");
        std::fs::write(beads_dir.join("policy.yaml"), GATE_POLICY_YAML).expect("write policy");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue_with_status("bd-1", "Gated", status), "tester")
            .expect("create issue");
        drop(storage);
        db_path
    }

    #[test]
    fn close_blocked_when_required_gate_unsatisfied() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let db_path = setup_gate_repo(&temp, Status::Custom("in_review".to_string()));
        let _guard = DirGuard::new(temp.path());
        let ctx = OutputContext::from_flags(false, false, true);

        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        let err = execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect_err("close must be blocked by unsatisfied ci_green gate");
        assert!(
            matches!(err, BeadsError::PolicyViolation { .. }),
            "unexpected error: {err:?}"
        );

        // The issue must remain un-closed.
        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Custom("in_review".to_string())
        );
    }

    #[test]
    fn close_allowed_after_gate_passes() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let db_path = setup_gate_repo(&temp, Status::Custom("in_review".to_string()));
        {
            let storage = SqliteStorage::open(&db_path).expect("storage");
            storage
                .record_scoped_gate_result(
                    "bd-1",
                    "in_review",
                    0,
                    "closed",
                    "ci_green",
                    "ci",
                    true,
                    None,
                    "ci-bot",
                )
                .expect("record pass");
        }
        let _guard = DirGuard::new(temp.path());
        let ctx = OutputContext::from_flags(false, false, true);

        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close should succeed once ci_green passes");

        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed
        );
    }

    #[test]
    fn close_unaffected_when_transition_not_gated() {
        // The gate only guards `in_review -> closed`; closing an `open` issue is
        // an `open -> closed` move with no rule, so it must proceed.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let db_path = setup_gate_repo(&temp, Status::Open);
        let _guard = DirGuard::new(temp.path());
        let ctx = OutputContext::from_flags(false, false, true);

        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("open -> closed is not gated and must succeed");

        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed
        );
    }

    #[test]
    fn close_unaffected_with_no_policy_file() {
        // Backward-compat: no policy.yaml at all → close behaves as before.
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        {
            let mut storage = SqliteStorage::open(&db_path).expect("storage");
            storage
                .create_issue(
                    &make_issue_with_status(
                        "bd-1",
                        "Plain",
                        Status::Custom("in_review".to_string()),
                    ),
                    "tester",
                )
                .expect("create");
        }
        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-1".to_string()],
            reason: Some("done".to_string()),
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx)
            .expect("close must succeed with no policy file");
        let storage = SqliteStorage::open(&db_path).expect("reopen");
        assert_eq!(
            storage.get_issue("bd-1").unwrap().unwrap().status,
            Status::Closed
        );
    }
}
