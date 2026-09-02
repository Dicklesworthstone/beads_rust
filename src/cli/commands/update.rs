//! Update command implementation.

use super::create::read_description_file;
use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, finalize_batched_blocked_cache_refresh,
    preserve_blocked_cache_on_error, report_auto_flush_failure, resolve_issue_id,
    resolve_issue_ids, retry_mutation_with_jsonl_recovery, update_issues_atomically_with_recovery,
};
use crate::cli::UpdateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{format_status_label, format_type_label, sanitize_terminal_inline};
use crate::model::acceptance::{
    AcceptanceChecklist, AcceptanceEdit, AcceptanceItem, AcceptanceSelector,
};
use crate::model::{Issue, IssueType, Priority, Status};
use crate::output::OutputContext;
use crate::storage::{EventAttribution, IssueUpdate, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use crate::util::time::parse_flexible_timestamp;
use crate::validation::LabelValidator;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// JSON output structure for updated issues.
///
/// `assignee` is always emitted (null when unassigned) rather than being
/// skipped when absent. `br update --claim` sets the assignee, and an agent
/// must be able to confirm the claim landed from the response alone; a field
/// that disappears when unset would leave "not claimed" and "not reported"
/// indistinguishable and force a verification `br show` round trip (GitHub
/// issue #393).
#[derive(Debug, Serialize)]
struct UpdatedIssueOutput {
    id: String,
    title: String,
    status: String,
    priority: i32,
    // Coordination fields echoed so a caller can confirm a claim landed from
    // the update response alone, without a follow-up `br show` (#393).
    assignee: Option<String>,
    owner: Option<String>,
    updated_at: DateTime<Utc>,
    /// Resulting acceptance checklist, present only when the call used
    /// `--check-acceptance` / `--uncheck-acceptance` / `--add-acceptance`
    /// (GitHub #477). Lets the caller confirm the tick landed and read the
    /// remaining count without a follow-up `br show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    acceptance_criteria: Option<AcceptanceCriteriaOutput>,
    /// Resulting notes field, present only when the call used
    /// `--append-notes` (GitHub #480), so the caller can confirm what was
    /// appended without a follow-up `br show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<NotesAppendOutput>,
}

impl From<&Issue> for UpdatedIssueOutput {
    fn from(issue: &Issue) -> Self {
        Self {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status.as_str().to_string(),
            priority: issue.priority.0,
            assignee: issue.assignee.clone(),
            owner: issue.owner.clone(),
            updated_at: issue.updated_at,
            acceptance_criteria: None,
            notes: None,
        }
    }
}

/// Structured notes state after `--append-notes` (GitHub #480).
#[derive(Debug, Clone, Serialize)]
struct NotesAppendOutput {
    /// The block this call asked to append (several values joined by `\n`).
    appended: String,
    /// False when the field already ended with `appended`, in which case
    /// nothing was written (idempotent re-run).
    applied: bool,
    chars_before: usize,
    chars_after: usize,
    /// The complete resulting notes field.
    text: String,
}

/// Per-issue plan for `--append-notes`, computed from the issue's current
/// field before anything is written (GitHub #480).
#[derive(Debug, Clone)]
struct NotesAppendPlan {
    id: String,
    output: NotesAppendOutput,
}

/// Join the current notes and an appended block: the current bytes are kept
/// verbatim, the block starts on its own line, and no separator is invented
/// when the field is empty or already ends with a newline.
fn append_notes_body(current: &str, appended: &str) -> String {
    if current.is_empty() || current.ends_with('\n') {
        format!("{current}{appended}")
    } else {
        format!("{current}\n{appended}")
    }
}

/// True when `current` already ends with `appended` as a whole block (the
/// entire field, or preceded by a line break), so appending it again would
/// only duplicate the last append.
fn notes_already_end_with(current: &str, appended: &str) -> bool {
    current
        .strip_suffix(appended)
        .is_some_and(|head| head.is_empty() || head.ends_with('\n'))
}

fn plan_notes_appends(
    storage: &SqliteStorage,
    ids: &[String],
    args: &UpdateArgs,
) -> Result<Vec<NotesAppendPlan>> {
    if args.append_notes.is_empty() {
        return Ok(Vec::new());
    }
    if args.append_notes.iter().any(|text| text.trim().is_empty()) {
        return Err(BeadsError::validation(
            "append-notes",
            "text to append must not be empty",
        ));
    }
    let appended = args.append_notes.join("\n");

    let mut plans = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(issue) = storage.get_issue(id)? else {
            // Missing targets are reported by validate_mutable_target_issues.
            continue;
        };
        let current = issue.notes.unwrap_or_default();
        let chars_before = current.chars().count();
        let (applied, text) = if notes_already_end_with(&current, &appended) {
            (false, current)
        } else {
            (true, append_notes_body(&current, &appended))
        };
        let chars_after = text.chars().count();
        plans.push(NotesAppendPlan {
            id: id.clone(),
            output: NotesAppendOutput {
                appended: appended.clone(),
                applied,
                chars_before,
                chars_after,
                text,
            },
        });
    }
    Ok(plans)
}

/// Structured acceptance-checklist state after an in-place edit (GitHub #477).
#[derive(Debug, Clone, Serialize)]
struct AcceptanceCriteriaOutput {
    /// Every checklist item, in document order, with 1-based `index`.
    items: Vec<AcceptanceItem>,
    checked_count: usize,
    total: usize,
    remaining: usize,
    /// 1-based indexes this call requested to check.
    checked: Vec<usize>,
    /// 1-based indexes this call requested to uncheck.
    unchecked: Vec<usize>,
    /// 1-based indexes of the items this call appended.
    added: Vec<usize>,
}

impl AcceptanceCriteriaOutput {
    fn from_edit(edit: &AcceptanceEdit) -> Self {
        let checklist = AcceptanceChecklist::parse(&edit.body);
        let total = checklist.len();
        let checked_count = checklist.checked_count();
        Self {
            items: checklist.items(),
            checked_count,
            total,
            remaining: total.saturating_sub(checked_count),
            checked: edit.checked.clone(),
            unchecked: edit.unchecked.clone(),
            added: edit.added.clone(),
        }
    }
}

/// Per-issue plan for the in-place acceptance-checklist flags. Computed
/// before any write from the issue's current field value, so the whole
/// request is validated (out-of-range index, ambiguous text, no checklist)
/// and rejected as a unit when anything is wrong.
struct AcceptanceEditPlan {
    id: String,
    edit: AcceptanceEdit,
    /// False when every requested item was already in the requested state
    /// and nothing was appended: the field is then not rewritten at all.
    changed: bool,
}

fn parse_acceptance_selectors(flag: &str, raw: &[String]) -> Result<Vec<AcceptanceSelector>> {
    let mut selectors = Vec::new();
    for value in raw {
        selectors.extend(
            AcceptanceSelector::parse_list(value)
                .map_err(|reason| BeadsError::validation(flag, reason))?,
        );
    }
    Ok(selectors)
}

fn plan_acceptance_edits(
    storage: &SqliteStorage,
    ids: &[String],
    args: &UpdateArgs,
) -> Result<Vec<AcceptanceEditPlan>> {
    if args.check_acceptance.is_empty()
        && args.uncheck_acceptance.is_empty()
        && args.add_acceptance.is_empty()
    {
        return Ok(Vec::new());
    }
    let check_selectors = parse_acceptance_selectors("check-acceptance", &args.check_acceptance)?;
    let uncheck_selectors =
        parse_acceptance_selectors("uncheck-acceptance", &args.uncheck_acceptance)?;

    let mut plans = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(issue) = storage.get_issue(id)? else {
            // Missing targets are reported by validate_mutable_target_issues.
            continue;
        };
        let body = issue.acceptance_criteria.unwrap_or_default();
        let checklist = AcceptanceChecklist::parse(&body);
        if checklist.is_empty() && !(check_selectors.is_empty() && uncheck_selectors.is_empty()) {
            return Err(BeadsError::validation(
                "check-acceptance",
                format!(
                    "{id}: acceptance_criteria has no checklist items to tick; add items with \
                     --add-acceptance or set the field with --acceptance-criteria"
                ),
            ));
        }
        let resolve_all = |flag: &str, selectors: &[AcceptanceSelector]| -> Result<Vec<usize>> {
            selectors
                .iter()
                .map(|selector| {
                    checklist
                        .resolve(selector)
                        .map_err(|reason| BeadsError::validation(flag, format!("{id}: {reason}")))
                })
                .collect()
        };
        let check = resolve_all("check-acceptance", &check_selectors)?;
        let uncheck = resolve_all("uncheck-acceptance", &uncheck_selectors)?;
        let edit = checklist
            .edit(&check, &uncheck, &args.add_acceptance)
            .map_err(|reason| BeadsError::validation("acceptance", format!("{id}: {reason}")))?;
        let changed = edit.changed_from(&body);
        plans.push(AcceptanceEditPlan {
            id: id.clone(),
            edit,
            changed,
        });
    }
    Ok(plans)
}

/// Snapshot of which fields the caller explicitly requested to change and
/// the post-mutation values they produced, captured directly from the
/// validated pre-mutation `issue_before` + the `IssueUpdate` struct that was
/// applied.
///
/// We deliberately do NOT derive the post-mutation values from a second
/// `get_issue(id)` read after the write transaction commits.  Doing so has
/// surfaced as an "unrelated bead's fields leak into the diff" bug in the
/// wild (see issue #256): a rare, yet-to-be-fully-root-caused read-path
/// inconsistency (e.g. fsqlite prepared-statement / pager cache edge case,
/// or a concurrent external writer touching the JSONL between the two
/// reads) can cause the post-update `get_issue` to return data that belongs
/// to a different row while the on-disk write is still correct.
///
/// By pairing the pre-mutation snapshot (whose `id` is guarded by
/// `get_issue_from_conn`'s post-condition check to match the requested id)
/// with the exact `IssueUpdate` struct the user passed, the rendered diff
/// is guaranteed to reference only the target bead and only the fields the
/// user explicitly asked to change.  Ghost fields like `status: open →
/// closed` can no longer appear on a `--priority 1` no-op.
#[derive(Debug, Default, Clone)]
struct UpdateDiff {
    status: Option<(Status, Status)>,
    priority: Option<(Priority, Priority)>,
    issue_type: Option<(IssueType, IssueType)>,
    assignee: Option<(Option<String>, Option<String>)>,
    owner: Option<(Option<String>, Option<String>)>,
}

impl UpdateDiff {
    fn from_before_and_update(before: &Issue, update: &IssueUpdate) -> Self {
        let mut diff = Self::default();
        if let Some(ref new_status) = update.status
            && before.status != *new_status
        {
            diff.status = Some((before.status.clone(), new_status.clone()));
        }
        if let Some(new_priority) = update.priority
            && before.priority != new_priority
        {
            diff.priority = Some((before.priority, new_priority));
        }
        if let Some(ref new_type) = update.issue_type
            && before.issue_type != *new_type
        {
            diff.issue_type = Some((before.issue_type.clone(), new_type.clone()));
        }
        if let Some(ref new_assignee_opt) = update.assignee {
            let before_assignee = before.assignee.clone();
            if before_assignee != *new_assignee_opt {
                diff.assignee = Some((before_assignee, new_assignee_opt.clone()));
            }
        }
        if let Some(ref new_owner_opt) = update.owner {
            let before_owner = before.owner.clone();
            if before_owner != *new_owner_opt {
                diff.owner = Some((before_owner, new_owner_opt.clone()));
            }
        }
        diff
    }
}

#[derive(Debug)]
enum UpdateRenderItem {
    Summary {
        id: String,
        title: String,
        diff: Box<UpdateDiff>,
        /// Acceptance checklist state to print after the field diff when the
        /// call used the in-place acceptance flags (GitHub #477).
        acceptance: Option<Box<AcceptanceCriteriaOutput>>,
        /// Notes append result to print when the call used `--append-notes`
        /// (GitHub #480).
        notes: Option<Box<NotesAppendOutput>>,
    },
    NoUpdates {
        id: String,
    },
}

#[derive(Debug)]
struct UpdateRouteOutput {
    updated_issues: Vec<UpdatedIssueOutput>,
    render_items: Vec<UpdateRenderItem>,
    resolved_ids: Vec<String>,
    capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

#[derive(Debug, Serialize)]
struct UpdateWithCapacityWarnings {
    updated: Vec<UpdatedIssueOutput>,
    warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

enum ParentUpdatePlan {
    Unchanged,
    Clear,
    Set(String),
}

struct PreparedUpdateRoute {
    storage_ctx: config::OpenStorageResult,
    actor: String,
    resolved_ids: Vec<String>,
    update: IssueUpdate,
    has_updates: bool,
    add_labels: Vec<String>,
    remove_labels: Vec<String>,
    set_labels: bool,
    valid_set_labels: Vec<String>,
    resolved_parent: ParentUpdatePlan,
    /// In-place acceptance-checklist edits, one per target issue (GitHub #477).
    acceptance_edits: Vec<AcceptanceEditPlan>,
    /// `--append-notes` plans, one per target issue (GitHub #480).
    notes_appends: Vec<NotesAppendPlan>,
    /// Non-fatal notes from the #467/#481 overwrite guard: whole-field
    /// rewrites that keep the field's size but little of its content.
    overwrite_advisories: Vec<String>,
    auto_flush_external: bool,
    /// Tier 1 attribution (issue #312, Layer 3 capture-only) staged onto each
    /// mutation's audit events. Recorded only — never gated or enforced on.
    attribution: EventAttribution,
    _routed_write_lock: RoutedWorkspaceWriteLock,
}

/// Execute the update command.
///
/// # Errors
///
/// Returns an error if database operations fail or validation errors occur.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &UpdateArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    // Refuse terminal-state transitions before doing any I/O. `br update`
    // is a data-only field mutator; terminal-state transitions
    // (closed, tombstone) must go through their dedicated commands so the
    // close-policy / delete pipelines are applied (see beads_rust#301).
    reject_terminal_status_transition(args.status.as_deref())?;

    // Resolve description-file input once before route discovery/fan-out.
    // This is essential for `--description-file -`: stdin is a single stream
    // and must not be consumed independently by each routed repository.
    let resolved_args = resolve_update_description(args)?;
    let args = &resolved_args;

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

    let (updated_issues, render_items, ordered_resolved_ids, mut capacity_warnings) =
        if routed_batches.iter().any(|batch| batch.is_external) {
            let normalized_local_beads_dir =
                dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
            let mut prepared_routes = Vec::new();
            let mut routed_updated_issues = Vec::new();
            let mut routed_render_items = Vec::new();
            let mut routed_resolved_ids = Vec::new();
            let mut routed_capacity_warnings = Vec::new();
            for batch in routed_batches {
                let mut batch_args = args.clone();
                batch_args.ids.clone_from(&batch.issue_inputs);

                let normalized_batch_beads_dir = dunce::canonicalize(&batch.beads_dir)
                    .unwrap_or_else(|_| batch.beads_dir.clone());
                let mut batch_cli = cli.clone();
                // Routed projects must resolve their own metadata-defined DB path
                // instead of being forced back to the local override. Preserve the
                // caller's explicit DB only for the local batch.
                batch_cli.db = if normalized_batch_beads_dir == normalized_local_beads_dir {
                    cli.db.clone()
                } else {
                    None
                };
                prepared_routes.push((
                    batch.issue_inputs.clone(),
                    prepare_single_route(
                        &batch_args,
                        &batch_cli,
                        &batch.beads_dir,
                        batch.is_external,
                    )?,
                ));
            }

            let all_resolved_ids = prepared_routes
                .iter()
                .flat_map(|(_, route)| route.resolved_ids.iter().cloned())
                .collect::<Vec<_>>();
            validate_multi_issue_external_ref_update(
                args.external_ref.as_deref(),
                &all_resolved_ids,
            )?;

            let use_machine_output = update_uses_machine_output(ctx);
            let use_human_output = update_uses_human_output(ctx);

            for (issue_inputs, prepared_route) in prepared_routes {
                let route_output = execute_prepared_route(prepared_route, ctx)?;

                routed_capacity_warnings.extend(route_output.capacity_warnings);

                if use_machine_output {
                    routed_updated_issues.push((issue_inputs.clone(), route_output.updated_issues));
                } else if use_human_output {
                    routed_render_items.push((issue_inputs.clone(), route_output.render_items));
                }
                routed_resolved_ids.push((issue_inputs, route_output.resolved_ids));
            }

            let updated_issues = if use_machine_output {
                reorder_routed_items_by_requested_inputs(
                    &target_inputs,
                    routed_updated_issues,
                    "update routing",
                )?
            } else {
                Vec::new()
            };
            let render_items = if use_human_output {
                reorder_routed_items_by_requested_inputs(
                    &target_inputs,
                    routed_render_items,
                    "update routing",
                )?
            } else {
                Vec::new()
            };
            let ordered_resolved_ids = reorder_routed_items_by_requested_inputs(
                &target_inputs,
                routed_resolved_ids,
                "update routing",
            )?;
            (
                updated_issues,
                render_items,
                ordered_resolved_ids,
                routed_capacity_warnings,
            )
        } else {
            let route_output =
                execute_prepared_route(prepare_single_route(args, cli, &beads_dir, false)?, ctx)?;
            (
                route_output.updated_issues,
                route_output.render_items,
                route_output.resolved_ids,
                route_output.capacity_warnings,
            )
        };

    let request_order = ordered_resolved_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect::<HashMap<_, _>>();
    capacity_warnings.sort_by(|left, right| {
        request_order
            .get(left.issue_id.as_str())
            .unwrap_or(&usize::MAX)
            .cmp(
                request_order
                    .get(right.issue_id.as_str())
                    .unwrap_or(&usize::MAX),
            )
            .then_with(|| left.capacity_kind.cmp(&right.capacity_kind))
            .then_with(|| left.capacity_name.cmp(&right.capacity_name))
    });

    if let Some(last_id) = ordered_resolved_ids.last() {
        crate::util::set_last_touched_id(&beads_dir, last_id);
    }

    if ctx.is_toon() {
        if capacity_warnings.is_empty() {
            ctx.toon(&updated_issues);
        } else {
            ctx.toon(&UpdateWithCapacityWarnings {
                updated: updated_issues,
                warnings: capacity_warnings,
            });
        }
    } else if ctx.is_json() {
        if capacity_warnings.is_empty() {
            ctx.json_pretty(&updated_issues);
        } else {
            ctx.json_pretty(&UpdateWithCapacityWarnings {
                updated: updated_issues,
                warnings: capacity_warnings,
            });
        }
    } else if !ctx.is_quiet() {
        print_render_items(&render_items);
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
        // beads_rust#297: emit inherited governing context for any
        // bead that just transitioned into in_progress (via --claim or
        // --status in_progress). Done after the update summary so the
        // child's status change is visible first, then the inherited
        // context the agent should be operating under.
        emit_inherited_context_for_in_progress_transitions(&beads_dir, cli, &render_items);
    }

    Ok(())
}

fn emit_inherited_context_for_in_progress_transitions(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    render_items: &[UpdateRenderItem],
) {
    if !crate::inheritance::is_enabled(beads_dir) {
        return;
    }
    let claimed_ids: Vec<&str> = render_items
        .iter()
        .filter_map(|item| match item {
            UpdateRenderItem::Summary { id, diff, .. }
                if diff
                    .status
                    .as_ref()
                    .is_some_and(|(_, new)| matches!(new, Status::InProgress)) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    if claimed_ids.is_empty() {
        return;
    }
    // Open a transient read-only storage to walk ancestry. Failure
    // here is non-fatal — the update has already succeeded and the
    // child's status change is already printed.
    let Ok(storage_ctx) = config::open_storage_with_cli(beads_dir, cli) else {
        return;
    };
    let storage = &storage_ctx.storage;
    for id in claimed_ids {
        let blocks = match crate::inheritance::collect_inherited_blocks(storage, id) {
            Ok(blocks) if !blocks.is_empty() => blocks,
            _ => continue,
        };
        let rendered = crate::inheritance::render_text(&blocks);
        println!();
        print!("{rendered}");
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_single_route(
    args: &UpdateArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<PreparedUpdateRoute> {
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
    let resolver = build_resolver(&config_layer, &storage_ctx.storage);
    let resolved_ids = resolve_target_ids(args, beads_dir, &resolver, &storage_ctx.storage)?;

    let claim_exclusive = config::claim_exclusive_from_layer(&config_layer);
    let update = build_update(args, &actor, claim_exclusive)?;

    // Strict status-workflow enforcement (issue #311) + transition rules
    // (issue #312, layer 1). When the project's `.beads/policy.yaml` configures
    // `workflow.strict: true` with a non-empty `workflow.statuses` set, a target
    // status outside that set is rejected. When `workflow.strict: true` with a
    // non-empty `workflow.transitions` map, a `from -> to` status change that is
    // not an allowed transition is rejected. Absent/non-strict workflow config
    // is a no-op, so existing repos are unaffected.
    if let Some(new_status) = update.status.as_ref() {
        let policy = crate::close_policy::load_for_beads_dir(beads_dir)?;
        policy.workflow.validate_status(new_status.as_str())?;
        let transitions_enforced = policy.workflow.transitions_enforced();
        if transitions_enforced {
            for id in &resolved_ids {
                // The current status is the `from` state for the transition
                // check. An issue that cannot be read (missing/unresolved)
                // validates against the `initial` key (from = None), mirroring
                // a create.
                let current = storage_ctx
                    .storage
                    .get_issue(id)?
                    .map(|issue| issue.status.as_str().to_string());
                if transitions_enforced {
                    policy
                        .workflow
                        .validate_transition(current.as_deref(), new_status.as_str())?;
                }
            }
        }
    }
    let has_updates = !update.is_empty()
        || !args.add_label.is_empty()
        || !args.remove_label.is_empty()
        || !args.set_labels.is_empty()
        || args.parent.is_some()
        || !args.check_acceptance.is_empty()
        || !args.uncheck_acceptance.is_empty()
        || !args.add_acceptance.is_empty()
        || !args.append_notes.is_empty();

    validate_mutable_target_issues(&storage_ctx.storage, &resolved_ids, has_updates)?;
    let overwrite_advisories = validate_text_field_overwrite_guard(
        &storage_ctx.storage,
        &resolved_ids,
        &update,
        args.force,
    )?;
    // In-place checklist edits and notes appends are planned from the
    // current field value and deliberately sit outside the #467 overwrite
    // guard: they cannot destroy content, so they must never need `--force`
    // (GitHub #477, #480).
    let acceptance_edits = plan_acceptance_edits(&storage_ctx.storage, &resolved_ids, args)?;
    let notes_appends = plan_notes_appends(&storage_ctx.storage, &resolved_ids, args)?;

    // Validate labels before making any database changes
    for label in &args.add_label {
        LabelValidator::validate(label).map_err(|e| BeadsError::validation("label", e.message))?;
    }
    for label in &args.remove_label {
        LabelValidator::validate(label).map_err(|e| BeadsError::validation("label", e.message))?;
    }

    let mut valid_set_labels = Vec::new();
    if !args.set_labels.is_empty() {
        let combined = args.set_labels.join(",");
        for label in combined.split(',') {
            let label = label.trim();
            if !label.is_empty() {
                LabelValidator::validate(label)
                    .map_err(|e| BeadsError::validation("label", e.message))?;
                valid_set_labels.push(label.to_string());
            }
        }
    }

    let resolved_parent =
        resolve_parent_update(args.parent.as_deref(), &resolver, &storage_ctx.storage)?;
    validate_parent_updates(&storage_ctx.storage, &resolved_ids, &resolved_parent)?;

    validate_transition_to_in_progress(&storage_ctx.storage, &resolved_ids, args)?;
    validate_route_runtime_guards(&storage_ctx.storage, &resolved_ids, &update)?;

    Ok(PreparedUpdateRoute {
        storage_ctx,
        actor,
        resolved_ids,
        update,
        has_updates,
        add_labels: args.add_label.clone(),
        remove_labels: args.remove_label.clone(),
        set_labels: !args.set_labels.is_empty(),
        valid_set_labels,
        resolved_parent,
        acceptance_edits,
        notes_appends,
        overwrite_advisories,
        auto_flush_external,
        attribution: EventAttribution::new(
            args.agent_name.as_deref(),
            args.harness.as_deref(),
            args.model.as_deref(),
            super::session_attribution_from_env().as_deref(),
        ),
        _routed_write_lock: routed_write_lock,
    })
}

#[allow(clippy::too_many_lines)]
fn execute_prepared_route(
    mut prepared: PreparedUpdateRoute,
    ctx: &OutputContext,
) -> Result<UpdateRouteOutput> {
    if can_use_bulk_label_only_route(&prepared) {
        return execute_bulk_label_only_route(prepared, ctx);
    }

    for advisory in &prepared.overwrite_advisories {
        ctx.warning(advisory);
    }

    let mut updated_issues: Vec<UpdatedIssueOutput> = Vec::new();
    let mut render_items = Vec::new();
    let resolved_ids = prepared.resolved_ids.clone();
    let use_machine_output = update_uses_machine_output(ctx);
    let use_human_output = update_uses_human_output(ctx);
    let mut route_has_mutated = false;
    let mut blocked_cache_dirty = false;
    let defer_blocked_cache_rebuild = prepared.update.status.is_some()
        || !matches!(prepared.resolved_parent, ParentUpdatePlan::Unchanged);
    let parent_changes_cache = !matches!(prepared.resolved_parent, ParentUpdatePlan::Unchanged);

    // Snapshot every row before the atomic field-update transaction. Human
    // diffs are derived from these validated snapshots, preserving the #256
    // defense while allowing the whole status batch to commit or roll back as
    // one unit.
    let mut issues_before = HashMap::with_capacity(prepared.resolved_ids.len());
    for id in &prepared.resolved_ids {
        let issue_before_result = prepared.storage_ctx.storage.get_issue(id);
        let issue_before = preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            false,
            "update",
            issue_before_result,
        )?;
        issues_before.insert(id.clone(), issue_before);
    }

    // Acceptance-checklist edits are per issue (each was planned from that
    // issue's own field), so they are merged into the shared field update
    // per id rather than cloned uniformly (GitHub #477).
    let acceptance_edits: HashMap<String, AcceptanceEditPlan> =
        std::mem::take(&mut prepared.acceptance_edits)
            .into_iter()
            .map(|plan| (plan.id.clone(), plan))
            .collect();
    let has_acceptance_writes = acceptance_edits.values().any(|plan| plan.changed);
    // Notes appends are likewise per issue: each was joined onto that
    // issue's own current field (GitHub #480).
    let notes_appends: HashMap<String, NotesAppendPlan> =
        std::mem::take(&mut prepared.notes_appends)
            .into_iter()
            .map(|plan| (plan.id.clone(), plan))
            .collect();
    let has_notes_writes = notes_appends.values().any(|plan| plan.output.applied);

    let mut capacity_warnings = Vec::new();
    if !prepared.update.is_empty() || has_acceptance_writes || has_notes_writes {
        let mut issue_update = prepared.update.clone();
        issue_update.skip_cache_rebuild = defer_blocked_cache_rebuild;
        let atomic_updates = prepared
            .resolved_ids
            .iter()
            .filter_map(|id| {
                let mut per_issue = issue_update.clone();
                if let Some(plan) = acceptance_edits.get(id).filter(|plan| plan.changed) {
                    per_issue.acceptance_criteria = Some(Some(plan.edit.body.clone()));
                }
                if let Some(plan) = notes_appends.get(id).filter(|plan| plan.output.applied) {
                    per_issue.notes = Some(Some(plan.output.text.clone()));
                }
                (!per_issue.is_empty()).then(|| (id.clone(), per_issue))
            })
            .collect::<Vec<_>>();

        prepared
            .storage_ctx
            .storage
            .set_pending_event_attribution(prepared.attribution.clone());
        let update_result = update_issues_atomically_with_recovery(
            &mut prepared.storage_ctx,
            true,
            "update",
            &atomic_updates,
            &prepared.actor,
        );
        preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            false,
            "update",
            update_result,
        )?;
        capacity_warnings = prepared.storage_ctx.storage.take_capacity_warnings();
        if prepared.update.status.is_some() {
            blocked_cache_dirty = true;
        }
        route_has_mutated = true;
    }

    for id in &prepared.resolved_ids {
        let issue_before = issues_before.remove(id).flatten();

        // Apply labels
        for label in &prepared.add_labels {
            let add_label_result = retry_mutation_with_jsonl_recovery(
                &mut prepared.storage_ctx,
                !route_has_mutated,
                "update label add",
                Some(id.as_str()),
                |storage| storage.add_label(id, label, &prepared.actor),
            );
            preserve_blocked_cache_on_error(
                &mut prepared.storage_ctx.storage,
                blocked_cache_dirty,
                "update",
                add_label_result,
            )?;
            route_has_mutated = true;
        }
        for label in &prepared.remove_labels {
            let remove_label_result = retry_mutation_with_jsonl_recovery(
                &mut prepared.storage_ctx,
                !route_has_mutated,
                "update label remove",
                Some(id.as_str()),
                |storage| storage.remove_label(id, label, &prepared.actor),
            );
            preserve_blocked_cache_on_error(
                &mut prepared.storage_ctx.storage,
                blocked_cache_dirty,
                "update",
                remove_label_result,
            )?;
            route_has_mutated = true;
        }
        if prepared.set_labels {
            let set_labels_result = retry_mutation_with_jsonl_recovery(
                &mut prepared.storage_ctx,
                !route_has_mutated,
                "update label set",
                Some(id.as_str()),
                |storage| storage.set_labels(id, &prepared.valid_set_labels, &prepared.actor),
            );
            preserve_blocked_cache_on_error(
                &mut prepared.storage_ctx.storage,
                blocked_cache_dirty,
                "update",
                set_labels_result,
            )?;
            route_has_mutated = true;
        }

        // Apply parent
        let parent_result = apply_parent_update(
            &mut prepared.storage_ctx,
            !route_has_mutated,
            id,
            &prepared.resolved_parent,
            &prepared.actor,
            defer_blocked_cache_rebuild,
        );
        preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            blocked_cache_dirty,
            "update",
            parent_result,
        )?;
        if parent_changes_cache {
            route_has_mutated = true;
            blocked_cache_dirty = true;
        }

        // Re-read post-mutation state for JSON/TOON machine output only.
        // For human-readable diff rendering we synthesize the diff from
        // `(issue_before, update)` below instead of trusting a second read,
        // to defend against the "unrelated bead's fields leak into diff"
        // regression reported in issue #256.
        let issue_after_result = prepared.storage_ctx.storage.get_issue(id);
        let issue_after = preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            blocked_cache_dirty,
            "update",
            issue_after_result,
        )?;

        // Derived from the planned edit (the exact bytes we wrote), not from
        // the post-write re-read, for the same #256 reason as the diff below.
        let acceptance_state = acceptance_edits
            .get(id)
            .map(|plan| AcceptanceCriteriaOutput::from_edit(&plan.edit));
        let notes_state = notes_appends.get(id).map(|plan| plan.output.clone());

        if use_machine_output {
            if let Some(ref issue) = issue_after {
                let mut output = UpdatedIssueOutput::from(issue);
                output.acceptance_criteria = acceptance_state;
                output.notes = notes_state;
                updated_issues.push(output);
            }
        } else if use_human_output && prepared.has_updates {
            // Derive the rendered title and diff from the validated
            // pre-mutation snapshot + the user's requested update.  If a
            // title change was requested use the requested new title, else
            // fall back to the authoritative `issue_before.title` (whose
            // row id has been post-condition-checked to equal `id`).  Only
            // if we have no `issue_before` at all (it was deleted / did
            // not exist before our write, which should not happen on the
            // `update` command path) do we fall back to the post-read.
            let title = prepared
                .update
                .title
                .clone()
                .or_else(|| issue_before.as_ref().map(|b| b.title.clone()))
                .or_else(|| issue_after.as_ref().map(|i| i.title.clone()))
                .unwrap_or_default();
            let diff = issue_before
                .as_ref()
                .map_or_else(UpdateDiff::default, |before| {
                    UpdateDiff::from_before_and_update(before, &prepared.update)
                });
            render_items.push(UpdateRenderItem::Summary {
                id: id.clone(),
                title,
                diff: Box::new(diff),
                acceptance: acceptance_state.map(Box::new),
                notes: notes_state.map(Box::new),
            });
        } else if use_human_output {
            render_items.push(UpdateRenderItem::NoUpdates { id: id.clone() });
        }
    }

    if defer_blocked_cache_rebuild && blocked_cache_dirty {
        finalize_batched_blocked_cache_refresh(
            &mut prepared.storage_ctx.storage,
            blocked_cache_dirty,
            "update",
        )?;
    }

    prepared.storage_ctx.flush_no_db_if_dirty()?;
    if prepared.auto_flush_external
        && let Err(error) = prepared.storage_ctx.auto_flush_if_enabled()
    {
        report_auto_flush_failure(
            ctx,
            &prepared.storage_ctx.paths.beads_dir,
            &prepared.storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(UpdateRouteOutput {
        updated_issues,
        render_items,
        resolved_ids,
        capacity_warnings,
    })
}

fn can_use_bulk_label_only_route(prepared: &PreparedUpdateRoute) -> bool {
    let add_only = !prepared.add_labels.is_empty() && prepared.remove_labels.is_empty();
    let remove_only = prepared.add_labels.is_empty() && !prepared.remove_labels.is_empty();

    (add_only || remove_only)
        && prepared.update.is_empty()
        && prepared.acceptance_edits.is_empty()
        && prepared.notes_appends.is_empty()
        && !prepared.set_labels
        && matches!(prepared.resolved_parent, ParentUpdatePlan::Unchanged)
}

fn execute_bulk_label_only_route(
    mut prepared: PreparedUpdateRoute,
    ctx: &OutputContext,
) -> Result<UpdateRouteOutput> {
    let resolved_ids = prepared.resolved_ids.clone();
    let actor = prepared.actor.clone();
    let add_labels = prepared.add_labels.clone();
    let remove_labels = prepared.remove_labels.clone();
    let mut route_has_mutated = false;

    for label in add_labels {
        let add_label_result = retry_mutation_with_jsonl_recovery(
            &mut prepared.storage_ctx,
            !route_has_mutated,
            "bulk update label add",
            None,
            |storage| storage.add_label_to_issues_bulk(&resolved_ids, &label, &actor),
        );
        let _changed_ids = preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            false,
            "update",
            add_label_result,
        )?;
        route_has_mutated = true;
    }

    for label in remove_labels {
        let remove_label_result = retry_mutation_with_jsonl_recovery(
            &mut prepared.storage_ctx,
            !route_has_mutated,
            "bulk update label remove",
            None,
            |storage| storage.remove_label_from_issues_bulk(&resolved_ids, &label, &actor),
        );
        let _changed_ids = preserve_blocked_cache_on_error(
            &mut prepared.storage_ctx.storage,
            false,
            "update",
            remove_label_result,
        )?;
        route_has_mutated = true;
    }

    let issues = prepared
        .storage_ctx
        .storage
        .get_issues_by_ids(&resolved_ids)?;
    let issues_by_id = issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect::<HashMap<_, _>>();

    let use_machine_output = update_uses_machine_output(ctx);
    let use_human_output = update_uses_human_output(ctx);
    let mut updated_issues = Vec::new();
    let mut render_items = Vec::new();

    for id in &resolved_ids {
        let issue = issues_by_id.get(id);
        if use_machine_output {
            if let Some(issue) = issue {
                updated_issues.push(UpdatedIssueOutput::from(issue));
            }
        } else if use_human_output && prepared.has_updates {
            render_items.push(UpdateRenderItem::Summary {
                id: id.clone(),
                title: issue.map_or_else(String::new, |issue| issue.title.clone()),
                diff: Box::new(UpdateDiff::default()),
                acceptance: None,
                notes: None,
            });
        } else if use_human_output {
            render_items.push(UpdateRenderItem::NoUpdates { id: id.clone() });
        }
    }

    prepared.storage_ctx.flush_no_db_if_dirty()?;
    if prepared.auto_flush_external
        && let Err(error) = prepared.storage_ctx.auto_flush_if_enabled()
    {
        report_auto_flush_failure(
            ctx,
            &prepared.storage_ctx.paths.beads_dir,
            &prepared.storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(UpdateRouteOutput {
        updated_issues,
        render_items,
        resolved_ids,
        capacity_warnings: Vec::new(),
    })
}

fn update_uses_machine_output(ctx: &OutputContext) -> bool {
    ctx.is_json() || ctx.is_toon()
}

fn update_uses_human_output(ctx: &OutputContext) -> bool {
    !ctx.is_quiet() && !update_uses_machine_output(ctx)
}

fn validate_multi_issue_external_ref_update(
    external_ref: Option<&str>,
    resolved_ids: &[String],
) -> Result<()> {
    let Some(external_ref) = external_ref.filter(|value| !value.is_empty()) else {
        return Ok(());
    };

    let distinct_ids = resolved_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if distinct_ids.len() > 1 {
        return Err(BeadsError::validation(
            "external_ref",
            format!(
                "cannot set external_ref '{external_ref}' on multiple issues in a single update"
            ),
        ));
    }

    Ok(())
}

fn validate_route_runtime_guards(
    storage: &SqliteStorage,
    resolved_ids: &[String],
    update: &IssueUpdate,
) -> Result<()> {
    if update.expect_unassigned {
        let claim_actor = update.claim_actor.as_deref().unwrap_or("");
        for id in resolved_ids {
            let issue = storage
                .get_issue(id)?
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.clone() })?;
            let trimmed = issue
                .assignee
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());

            match trimmed {
                None => {}
                Some(current) if !update.claim_exclusive && current == claim_actor => {}
                Some(current) => {
                    return Err(BeadsError::validation(
                        "claim",
                        format!("issue {id} already assigned to {current}"),
                    ));
                }
            }
        }
    }

    validate_multi_issue_external_ref_update(
        update
            .external_ref
            .as_ref()
            .and_then(|value| value.as_deref()),
        resolved_ids,
    )?;

    if let Some(Some(external_ref)) = &update.external_ref
        && let Some(existing_issue) = storage.find_by_external_ref(external_ref)?
        && existing_issue.id != resolved_ids.first().map_or("", String::as_str)
    {
        return Err(BeadsError::Config(format!(
            "External reference '{external_ref}' already exists on issue {}",
            existing_issue.id
        )));
    }

    Ok(())
}

fn validate_transition_to_in_progress(
    storage: &SqliteStorage,
    ids: &[String],
    args: &UpdateArgs,
) -> Result<()> {
    let transitioning_to_in_progress = args.claim
        || args
            .status
            .as_ref()
            .is_some_and(|status| status.eq_ignore_ascii_case("in_progress"));

    if !transitioning_to_in_progress || args.force {
        return Ok(());
    }

    for id in ids {
        // Use start-blockers (not `is_blocked`), so an epic that is only
        // "blocked" by its own still-open children — a close-ordering rollup,
        // not a real dependency — can still be claimed and worked on (#315).
        let blockers = storage.get_start_blockers(id)?;
        if !blockers.is_empty() {
            return Err(BeadsError::validation(
                "claim",
                format!("cannot claim blocked issue: {}", blockers.join(", ")),
            ));
        }
    }

    Ok(())
}

/// Print a summary of what changed for the issue.
fn print_update_summary(id: &str, title: &str, diff: &UpdateDiff) {
    println!("{}", updated_issue_human_line(id, title));

    if let Some((old_status, new_status)) = &diff.status {
        println!(
            "  status: {} → {}",
            format_status_label(old_status, false),
            format_status_label(new_status, false)
        );
    }
    if let Some((old_priority, new_priority)) = &diff.priority {
        println!("  priority: P{} → P{}", old_priority.0, new_priority.0);
    }
    if let Some((old_type, new_type)) = &diff.issue_type {
        println!(
            "  type: {} → {}",
            format_type_label(old_type),
            format_type_label(new_type)
        );
    }
    if let Some((old_assignee, new_assignee)) = &diff.assignee {
        let before_assignee = old_assignee.as_deref().map_or_else(
            || "(none)".to_string(),
            |value| sanitize_terminal_inline(value).into_owned(),
        );
        let after_assignee = new_assignee.as_deref().map_or_else(
            || "(none)".to_string(),
            |value| sanitize_terminal_inline(value).into_owned(),
        );
        println!("  assignee: {before_assignee} → {after_assignee}");
    }
    if let Some((old_owner, new_owner)) = &diff.owner {
        let before_owner = old_owner.as_deref().map_or_else(
            || "(none)".to_string(),
            |value| sanitize_terminal_inline(value).into_owned(),
        );
        let after_owner = new_owner.as_deref().map_or_else(
            || "(none)".to_string(),
            |value| sanitize_terminal_inline(value).into_owned(),
        );
        println!("  owner: {before_owner} → {after_owner}");
    }
}

fn updated_issue_human_line(id: &str, title: &str) -> String {
    format!(
        "Updated {}: {}",
        sanitize_terminal_inline(id),
        sanitize_terminal_inline(title)
    )
}

fn no_updates_human_line(id: &str) -> String {
    format!("No updates specified for {}", sanitize_terminal_inline(id))
}

fn issue_input_text(input: &str) -> String {
    sanitize_terminal_inline(input).into_owned()
}

/// Render the acceptance checklist after an in-place edit (GitHub #477):
/// every item with its 1-based index and box state, then a summary line
/// naming what this call changed and how many items remain. Printing the
/// resulting state means the caller never needs a follow-up `br show` and
/// never infers success from the exit code alone.
fn print_acceptance_state(id: &str, state: &AcceptanceCriteriaOutput) {
    let plural = if state.total == 1 { "" } else { "s" };
    println!("{id} acceptance criteria ({} item{plural})", state.total);
    let width = state.total.to_string().len();
    for item in &state.items {
        println!(
            "  [{}] {:<width$}  {}",
            if item.checked { 'x' } else { ' ' },
            item.index,
            sanitize_terminal_inline(&item.text),
        );
    }
    let list = |indexes: &[usize]| {
        indexes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut parts = Vec::new();
    if !state.checked.is_empty() {
        parts.push(format!(
            "checked {} ({})",
            state.checked.len(),
            list(&state.checked)
        ));
    }
    if !state.unchecked.is_empty() {
        parts.push(format!(
            "unchecked {} ({})",
            state.unchecked.len(),
            list(&state.unchecked)
        ));
    }
    if !state.added.is_empty() {
        parts.push(format!(
            "added {} ({})",
            state.added.len(),
            list(&state.added)
        ));
    }
    parts.push(format!(
        "{} of {} complete",
        state.checked_count, state.total
    ));
    parts.push(format!("{} remaining", state.remaining));
    println!("{}", parts.join(" · "));
}

/// Render the result of `--append-notes` (GitHub #480): what was appended and
/// the field's size afterwards, or that the append was skipped because the
/// field already ended with that text.
fn print_notes_append_state(state: &NotesAppendOutput) {
    let lines = state.text.lines().count();
    let plural = if lines == 1 { "" } else { "s" };
    if state.applied {
        println!(
            "  notes: appended {} chars (now {lines} line{plural}, {} chars)",
            state.appended.chars().count(),
            state.chars_after
        );
    } else {
        println!(
            "  notes: already ends with the requested text; nothing appended ({lines} line{plural}, {} chars)",
            state.chars_after
        );
    }
}

fn print_render_items(render_items: &[UpdateRenderItem]) {
    for item in render_items {
        match item {
            UpdateRenderItem::Summary {
                id,
                title,
                diff,
                acceptance,
                notes,
            } => {
                print_update_summary(id, title, diff.as_ref());
                if let Some(state) = acceptance {
                    print_acceptance_state(id, state);
                }
                if let Some(state) = notes {
                    print_notes_append_state(state);
                }
            }
            UpdateRenderItem::NoUpdates { id } => println!("{}", no_updates_human_line(id)),
        }
    }
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
            return Err(BeadsError::Config(format!(
                "{context} produced mismatched issue/result counts"
            )));
        }

        for (input, item) in batch_inputs.into_iter().zip(batch_items) {
            let Some(index) = positions_by_input
                .get_mut(input.as_str())
                .and_then(VecDeque::pop_front)
            else {
                let input = issue_input_text(&input);
                return Err(BeadsError::Config(format!(
                    "{context} returned unexpected issue input {input}"
                )));
            };
            let Some(slot) = ordered_items.get_mut(index) else {
                let input = issue_input_text(&input);
                return Err(BeadsError::Config(format!(
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
                BeadsError::Config(format!("{context} did not produce a result for {input}"))
            })
        })
        .collect()
}

fn build_resolver(config_layer: &config::ConfigLayer, _storage: &SqliteStorage) -> IdResolver {
    let id_config = config::id_config_from_layer(config_layer);
    IdResolver::new(ResolverConfig::with_prefix(id_config.prefix))
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

    resolve_issue_ids(storage, resolver, &ids)
}

fn validate_mutable_target_issues(
    storage: &SqliteStorage,
    ids: &[String],
    has_updates: bool,
) -> Result<()> {
    if !has_updates {
        return Ok(());
    }

    for id in ids {
        if storage
            .get_issue(id)?
            .as_ref()
            .is_some_and(|issue| issue.status == Status::Tombstone)
        {
            return Err(BeadsError::validation(
                "issue",
                format!("cannot update tombstone issue: {id}"),
            ));
        }
    }

    Ok(())
}

/// Below this share of the current length, a replacement is treated as
/// destructive and refused without `--force` (GitHub #481). Expressed in
/// percent so the comparison stays in integer arithmetic.
const OVERWRITE_GUARD_MIN_LENGTH_PERCENT: usize = 50;

/// A replacement that passes the length tier but keeps fewer than this share
/// of the current field's words is allowed with an advisory naming both
/// lengths, so a wrong-variable rewrite of comparable size is still visible.
const OVERWRITE_GUARD_ADVISORY_RETENTION_PERCENT: usize = 50;

/// How the #467 overwrite guard classifies replacing a text field's current
/// value with an incoming one (GitHub #481).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverwriteTier {
    /// Empty field, or identical content: nothing can be lost.
    Safe,
    /// Keeps at least half the length and at least half the words.
    Revision,
    /// Keeps at least half the length but fewer than half the words: allowed,
    /// but reported.
    Rewrite {
        current_chars: usize,
        incoming_chars: usize,
        retained_words: usize,
        total_words: usize,
    },
    /// Clears the field or shrinks it below half its current length: refused
    /// without `--force`.
    Destructive {
        current_chars: usize,
        incoming_chars: usize,
    },
}

/// Count how many of `current`'s whitespace-separated words survive in
/// `incoming` (multiset intersection), returning `(retained, total)`.
/// Deterministic and linear, so it is safe on fields of any size.
fn retained_word_count(current: &str, incoming: &str) -> (usize, usize) {
    let mut available: HashMap<&str, usize> = HashMap::new();
    for word in incoming.split_whitespace() {
        *available.entry(word).or_insert(0) += 1;
    }
    let mut total = 0usize;
    let mut retained = 0usize;
    for word in current.split_whitespace() {
        total += 1;
        if let Some(count) = available.get_mut(word)
            && *count > 0
        {
            *count -= 1;
            retained += 1;
        }
    }
    (retained, total)
}

/// Classify one field replacement. The rule is deterministic and depends
/// only on the two values:
///
/// 1. empty current, or identical incoming: [`OverwriteTier::Safe`];
/// 2. incoming empty, or shorter than half the current length (in chars):
///    [`OverwriteTier::Destructive`];
/// 3. otherwise, fewer than half of the current words kept:
///    [`OverwriteTier::Rewrite`]; else [`OverwriteTier::Revision`].
fn classify_text_field_overwrite(current: &str, incoming: &str) -> OverwriteTier {
    if current.is_empty() || incoming == current {
        return OverwriteTier::Safe;
    }
    let current_chars = current.chars().count();
    let incoming_chars = incoming.chars().count();
    if incoming_chars * 100 < current_chars * OVERWRITE_GUARD_MIN_LENGTH_PERCENT {
        return OverwriteTier::Destructive {
            current_chars,
            incoming_chars,
        };
    }
    let (retained_words, total_words) = retained_word_count(current, incoming);
    if retained_words * 100 < total_words * OVERWRITE_GUARD_ADVISORY_RETENTION_PERCENT {
        return OverwriteTier::Rewrite {
            current_chars,
            incoming_chars,
            retained_words,
            total_words,
        };
    }
    OverwriteTier::Revision
}

/// Refuse to silently destroy a non-empty accumulating text field
/// (GitHub #467), proportionally to what the write would lose (GitHub #481).
///
/// `description`, `design`, `acceptance_criteria`, `notes`, and
/// `agent_context` build up over an issue's life and are frequently supplied
/// from shell variables in scripted/agent flows, where a typo, truncated
/// heredoc, or unset variable silently destroys the whole field. Those
/// failures all shrink the field severely, so that is the case that requires
/// `--force`: clearing it, or replacing it with content shorter than half its
/// current length. A revision that keeps at least half the length passes;
/// when it also keeps fewer than half the current words it is reported as an
/// advisory (returned here, printed by the caller) rather than refused.
/// Writing into an empty field and re-writing the identical value stay
/// silent so legitimate idempotent scripts keep working.
fn validate_text_field_overwrite_guard(
    storage: &SqliteStorage,
    ids: &[String],
    update: &IssueUpdate,
    force: bool,
) -> Result<Vec<String>> {
    if force {
        return Ok(Vec::new());
    }
    let requested: [(&str, Option<&Option<String>>); 5] = [
        ("description", update.description.as_ref()),
        ("design", update.design.as_ref()),
        ("acceptance_criteria", update.acceptance_criteria.as_ref()),
        ("notes", update.notes.as_ref()),
        ("agent_context", update.agent_context.as_ref()),
    ];
    if requested.iter().all(|(_, value)| value.is_none()) {
        return Ok(Vec::new());
    }
    let mut violations = Vec::new();
    let mut advisories = Vec::new();
    for id in ids {
        let Some(issue) = storage.get_issue(id)? else {
            continue;
        };
        for (field, new_value) in &requested {
            let Some(new_value) = new_value else {
                continue;
            };
            let current = match *field {
                "description" => issue.description.as_deref(),
                "design" => issue.design.as_deref(),
                "acceptance_criteria" => issue.acceptance_criteria.as_deref(),
                "notes" => issue.notes.as_deref(),
                "agent_context" => issue.agent_context.as_deref(),
                _ => unreachable!(),
            }
            .unwrap_or("");
            let incoming = new_value.as_deref().unwrap_or("");
            match classify_text_field_overwrite(current, incoming) {
                OverwriteTier::Safe | OverwriteTier::Revision => {}
                OverwriteTier::Rewrite {
                    current_chars,
                    incoming_chars,
                    retained_words,
                    total_words,
                } => advisories.push(format!(
                    "{id}: replacing '{field}' ({current_chars} chars) with different content \
                     ({incoming_chars} chars) that keeps {retained_words} of {total_words} words; \
                     run `br show {id}` if that was not intended"
                )),
                OverwriteTier::Destructive {
                    current_chars,
                    incoming_chars,
                } => violations.push(format!(
                    "{id}: refusing to overwrite non-empty '{field}' ({current_chars} chars) with {} without --force",
                    if incoming_chars == 0 {
                        "an empty value".to_string()
                    } else {
                        format!(
                            "different content ({incoming_chars} chars, {}% of the current length)",
                            incoming_chars * 100 / current_chars
                        )
                    },
                )),
            }
        }
    }
    if violations.is_empty() {
        return Ok(advisories);
    }
    Err(BeadsError::validation_with_hint(
        "update",
        violations.join("\n"),
        "These fields accumulate context: a write that clears the field or keeps less than half its length needs --force; run `br show <id>` first to read what is there, or use --append-notes / --add-acceptance to extend instead of replace.",
    ))
}

/// Reject `br update --status <terminal>` and direct the user at the
/// dedicated command for that transition.
///
/// `br update` is a data-only field mutator. Terminal-state transitions
/// (`closed`, `tombstone`) own their own audit / policy pipelines:
///
/// * `closed`    → `br close`  (close-policy gates: close-reason, AC, attribution, ...)
/// * `tombstone` → `br delete` (tombstone metadata, dependency rewiring)
///
/// Allowing both paths to reach the same terminal state would give the
/// project two different audit contracts depending on which command the
/// operator reached for — see beads_rust#301 for the regression that
/// motivated this gate.
///
/// This deliberately runs *before* any I/O (route discovery, locking,
/// SQLite open) so a misuse fails instantly rather than after acquiring
/// the workspace write lock.
fn reject_terminal_status_transition(raw_status: Option<&str>) -> Result<()> {
    let Some(raw) = raw_status else {
        return Ok(());
    };
    let parsed: Status = raw.parse()?;
    match parsed {
        Status::Closed => Err(BeadsError::validation(
            "status",
            "refusing to close via `br update --status closed`: \
             terminal-state transitions must go through `br close` so close-policy \
             (close-reason / AC / attribution) is enforced. \
             Use `br close <id> --reason \"...\"` instead, or `br close <id> \
             --bypass-policy --bypass-reason \"...\"` to opt out explicitly. \
             See https://github.com/Dicklesworthstone/beads_rust/issues/301.",
        )),
        Status::Tombstone => Err(BeadsError::validation(
            "status",
            "refusing to tombstone via `br update --status tombstone`: \
             use `br delete <id>` instead so dependency rewiring and tombstone \
             metadata are applied correctly.",
        )),
        _ => Ok(()),
    }
}

/// Resolve `br update`'s effective description before preparing any routes.
///
/// Clap enforces the inline/file conflict for CLI callers. This guard repeats
/// the contract for programmatic callers, then reads the file (or stdin)
/// verbatim exactly once so routed updates and JSONL-recovery retries reuse
/// the same captured value.
fn resolve_update_description(args: &UpdateArgs) -> Result<UpdateArgs> {
    if args.description.is_some() && args.description_file.is_some() {
        return Err(BeadsError::validation(
            "description_file",
            "cannot be combined with --description",
        ));
    }

    let Some(path) = args.description_file.as_deref() else {
        return Ok(args.clone());
    };

    let mut resolved = args.clone();
    resolved.description = Some(read_description_file(path)?);
    resolved.description_file = None;
    Ok(resolved)
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

    if args.session.is_some() && !matches!(status, Some(Status::Closed)) {
        return Err(BeadsError::validation(
            "session",
            "--session can only be used when closing with --status closed",
        ));
    }
    let (closed_at, close_reason, closed_by_session) = match &status {
        Some(Status::Closed) => (Some(Some(Utc::now())), None, args.session.clone().map(Some)),
        Some(_) => (Some(None), Some(None), Some(None)),
        None => (None, None, None),
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
        source_repo: optional_string_field(args.source_repo.as_deref()),
        source_repo_path: optional_string_field(args.source_repo_path.as_deref()),
        agent_context: agent_context_update_from_arg(args.agent_context.as_deref())?,
        closed_at,
        close_reason,
        closed_by_session,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        transition_comment: args.transition_comment.clone(),
        workflow_policy_bypass_reason: None,
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

/// Parse the `--agent-context` argument into an `IssueUpdate::agent_context`
/// payload. Accepts:
///
/// - `None` → don't touch the field (`Option<Option<String>>::None`).
/// - `Some("")` → clear the field back to NULL (`Some(None)`).
/// - `Some("@path")` → read the file at `path`; parse as YAML when the
///   extension is `.yaml`/`.yml`, otherwise as JSON. Normalize to JSON
///   so storage is opaque TEXT but always canonical-JSON-shaped.
/// - `Some("{...}")` → parse as JSON inline.
///
/// Validation happens here because the storage column is opaque TEXT —
/// without this guard we'd happily round-trip syntactically invalid
/// JSON through SQLite and then have the emission path discover the
/// problem at agent claim time. (beads_rust#297)
#[allow(clippy::option_option)]
pub(crate) fn agent_context_update_from_arg(value: Option<&str>) -> Result<Option<Option<String>>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Some(None));
    }

    let (source_label, body): (String, String) = if let Some(path_str) = raw.strip_prefix('@') {
        let path = std::path::Path::new(path_str);
        let contents = std::fs::read_to_string(path).map_err(|e| {
            BeadsError::Config(format!(
                "agent-context: cannot read {}: {e}",
                path.display()
            ))
        })?;
        let is_yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"));
        let normalized = if is_yaml {
            let value: serde_yml::Value = serde_yml::from_str(&contents).map_err(|e| {
                BeadsError::Config(format!(
                    "agent-context: YAML parse failed for {}: {e}",
                    path.display()
                ))
            })?;
            serde_json::to_string(&value).map_err(|e| {
                BeadsError::Config(format!(
                    "agent-context: YAML to JSON conversion failed for {}: {e}",
                    path.display()
                ))
            })?
        } else {
            let value: serde_json::Value = serde_json::from_str(&contents).map_err(|e| {
                BeadsError::Config(format!(
                    "agent-context: JSON parse failed for {}: {e}",
                    path.display()
                ))
            })?;
            serde_json::to_string(&value).map_err(|e| {
                BeadsError::Config(format!(
                    "agent-context: JSON re-serialization failed for {}: {e}",
                    path.display()
                ))
            })?
        };
        (path.display().to_string(), normalized)
    } else {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
            BeadsError::Config(format!(
                "agent-context: inline argument is not valid JSON: {e} (hint: use \
                 `--agent-context @path/to/instructions.yaml` for a file, or pass an \
                 empty string to clear)"
            ))
        })?;
        let normalized = serde_json::to_string(&value).map_err(|e| {
            BeadsError::Config(format!("agent-context: JSON re-serialization failed: {e}"))
        })?;
        ("<inline>".to_string(), normalized)
    };
    tracing::debug!(
        bytes = body.len(),
        source = %source_label,
        "agent-context: parsed and normalized to canonical JSON"
    );
    Ok(Some(Some(body)))
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

fn resolve_parent_update(
    parent: Option<&str>,
    resolver: &IdResolver,
    storage: &SqliteStorage,
) -> Result<ParentUpdatePlan> {
    match parent {
        None => Ok(ParentUpdatePlan::Unchanged),
        Some("") => Ok(ParentUpdatePlan::Clear),
        Some(parent_value) => {
            resolve_issue_id(storage, resolver, parent_value).map(ParentUpdatePlan::Set)
        }
    }
}

fn apply_parent_update(
    storage_ctx: &mut config::OpenStorageResult,
    allow_recovery: bool,
    issue_id: &str,
    parent: &ParentUpdatePlan,
    actor: &str,
    skip_cache_rebuild: bool,
) -> Result<()> {
    match parent {
        ParentUpdatePlan::Unchanged => Ok(()),
        ParentUpdatePlan::Clear => retry_mutation_with_jsonl_recovery(
            storage_ctx,
            allow_recovery,
            "update parent clear",
            Some(issue_id),
            |storage| storage.set_parent_with_options(issue_id, None, actor, skip_cache_rebuild),
        ),
        ParentUpdatePlan::Set(parent_id) => retry_mutation_with_jsonl_recovery(
            storage_ctx,
            allow_recovery,
            "update parent set",
            Some(issue_id),
            |storage| {
                storage.set_parent_with_options(
                    issue_id,
                    Some(parent_id),
                    actor,
                    skip_cache_rebuild,
                )
            },
        ),
    }
}

fn validate_parent_updates(
    storage: &SqliteStorage,
    issue_ids: &[String],
    parent: &ParentUpdatePlan,
) -> Result<()> {
    let ParentUpdatePlan::Set(parent_id) = parent else {
        return Ok(());
    };

    for issue_id in issue_ids {
        if issue_id == parent_id {
            return Err(BeadsError::SelfDependency {
                id: issue_id.clone(),
            });
        }

        if storage.would_create_parent_child_cycle(issue_id, parent_id, true)? {
            return Err(BeadsError::DependencyCycle {
                path: format!("Setting parent of {issue_id} to {parent_id} would create a cycle"),
            });
        }
    }

    Ok(())
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    parse_flexible_timestamp(s, "date")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CliOverrides;
    use crate::logging::init_test_logging;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::output::{OutputContext, OutputMode};
    use crate::storage::SqliteStorage;
    use chrono::{Datelike, Timelike};
    use std::fs;
    use tempfile::TempDir;
    use tracing::info;

    // === In-place acceptance checklist edits (GitHub #477) ===

    const ACCEPTANCE_FIXTURE: &str = "- [ ] schema migration applied\n\
- [ ] rollback path exercised\n\
- [ ] telemetry counter emitted\n";

    fn acceptance_workspace(acceptance: Option<&str>) -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let mut storage_ctx =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = Issue {
            id: "bd-ac".to_string(),
            title: "Acceptance target".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            description: Some("context that must survive".to_string()),
            acceptance_criteria: acceptance.map(str::to_string),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&issue, "tester")
            .expect("create issue");
        (temp, beads_dir)
    }

    fn acceptance_field(beads_dir: &Path) -> Issue {
        let storage_ctx =
            config::open_storage_with_cli(beads_dir, &CliOverrides::default()).expect("storage");
        storage_ctx
            .storage
            .get_issue("bd-ac")
            .expect("read issue")
            .expect("issue exists")
    }

    fn run_acceptance_update(beads_dir: &Path, args: &UpdateArgs) -> Result<UpdateRouteOutput> {
        let prepared = prepare_single_route(args, &CliOverrides::default(), beads_dir, false)?;
        execute_prepared_route(prepared, &OutputContext::from_flags(true, false, true))
    }

    #[test]
    fn check_acceptance_ticks_items_in_place_without_force() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some(ACCEPTANCE_FIXTURE));
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["1,3".to_string()],
            ..Default::default()
        };

        let output = run_acceptance_update(&beads_dir, &args).expect("tick without --force");

        let after = acceptance_field(&beads_dir);
        assert_eq!(
            after.acceptance_criteria.as_deref(),
            Some(
                "- [x] schema migration applied\n\
- [ ] rollback path exercised\n\
- [x] telemetry counter emitted\n"
            )
        );
        assert_eq!(
            after.description.as_deref(),
            Some("context that must survive")
        );
        let state = output.updated_issues[0]
            .acceptance_criteria
            .as_ref()
            .expect("acceptance state in machine output");
        assert_eq!(state.checked, vec![1, 3]);
        assert_eq!(state.checked_count, 2);
        assert_eq!(state.total, 3);
        assert_eq!(state.remaining, 1);
        assert_eq!(state.items[1].text, "rollback path exercised");
        assert!(!state.items[1].checked);
        let json = serde_json::to_value(&output.updated_issues[0]).unwrap();
        assert_eq!(json["acceptance_criteria"]["items"][0]["index"], 1);
        assert_eq!(json["acceptance_criteria"]["items"][0]["checked"], true);
        assert_eq!(json["acceptance_criteria"]["remaining"], 1);
    }

    #[test]
    fn check_acceptance_by_text_and_uncheck_by_index() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some(
            "- [x] schema migration applied\n- [ ] rollback path exercised\n",
        ));
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["ROLLBACK".to_string()],
            uncheck_acceptance: vec!["1".to_string()],
            ..Default::default()
        };

        let output = run_acceptance_update(&beads_dir, &args).expect("text selector + uncheck");

        assert_eq!(
            acceptance_field(&beads_dir).acceptance_criteria.as_deref(),
            Some("- [ ] schema migration applied\n- [x] rollback path exercised\n")
        );
        let state = output.updated_issues[0]
            .acceptance_criteria
            .as_ref()
            .unwrap();
        assert_eq!(state.checked, vec![2]);
        assert_eq!(state.unchecked, vec![1]);
    }

    #[test]
    fn check_acceptance_rejects_bad_requests_before_writing_anything() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some(ACCEPTANCE_FIXTURE));
        let before = acceptance_field(&beads_dir);

        // Out-of-range index in a batch: nothing applied, offender named.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["1,9".to_string()],
            ..Default::default()
        };
        let err = run_acceptance_update(&beads_dir, &args).unwrap_err();
        assert!(err.to_string().contains("item 9 does not exist"), "{err}");

        // Ambiguous text selector.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["e".to_string()],
            ..Default::default()
        };
        let err = run_acceptance_update(&beads_dir, &args).unwrap_err();
        assert!(
            err.to_string().contains("matches 3 acceptance items"),
            "{err}"
        );

        // Same item checked and unchecked.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["2".to_string()],
            uncheck_acceptance: vec!["rollback".to_string()],
            ..Default::default()
        };
        let err = run_acceptance_update(&beads_dir, &args).unwrap_err();
        assert!(
            err.to_string().contains("both checked and unchecked"),
            "{err}"
        );

        let after = acceptance_field(&beads_dir);
        assert_eq!(after.acceptance_criteria, before.acceptance_criteria);
        assert_eq!(after.updated_at, before.updated_at);
    }

    #[test]
    fn check_acceptance_without_a_checklist_is_an_error() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some("prose only, no boxes"));
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["1".to_string()],
            ..Default::default()
        };
        let err = run_acceptance_update(&beads_dir, &args).unwrap_err();
        assert!(err.to_string().contains("no checklist items"), "{err}");
        assert_eq!(
            acceptance_field(&beads_dir).acceptance_criteria.as_deref(),
            Some("prose only, no boxes")
        );
    }

    #[test]
    fn check_acceptance_already_checked_does_not_rewrite_the_row() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some("- [X] done\n- [ ] open\n"));
        let before = acceptance_field(&beads_dir);
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["done".to_string()],
            ..Default::default()
        };

        let output = run_acceptance_update(&beads_dir, &args).expect("idempotent tick");

        let after = acceptance_field(&beads_dir);
        assert_eq!(
            after.acceptance_criteria.as_deref(),
            Some("- [X] done\n- [ ] open\n")
        );
        assert_eq!(
            after.updated_at, before.updated_at,
            "no-op must not touch the row"
        );
        let state = output.updated_issues[0]
            .acceptance_criteria
            .as_ref()
            .expect("state still reported so the caller sees the checklist");
        assert_eq!(state.checked, vec![1]);
        assert_eq!(state.remaining, 1);
    }

    #[test]
    fn add_acceptance_appends_items_to_empty_and_populated_fields() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(None);
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            add_acceptance: vec!["first criterion".to_string()],
            ..Default::default()
        };
        let output = run_acceptance_update(&beads_dir, &args).expect("append to empty field");
        assert_eq!(
            acceptance_field(&beads_dir).acceptance_criteria.as_deref(),
            Some("- [ ] first criterion")
        );
        assert_eq!(
            output.updated_issues[0]
                .acceptance_criteria
                .as_ref()
                .unwrap()
                .added,
            vec![1]
        );

        // Append plus tick in one call: the tick indexes the existing list,
        // the new item lands unchecked at the end, and nothing else changes.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            add_acceptance: vec!["second criterion".to_string()],
            check_acceptance: vec!["1".to_string()],
            ..Default::default()
        };
        let output = run_acceptance_update(&beads_dir, &args).expect("append + tick");
        assert_eq!(
            acceptance_field(&beads_dir).acceptance_criteria.as_deref(),
            Some("- [x] first criterion\n- [ ] second criterion")
        );
        let state = output.updated_issues[0]
            .acceptance_criteria
            .as_ref()
            .unwrap();
        assert_eq!(state.added, vec![2]);
        assert_eq!(state.checked, vec![1]);
        assert_eq!(state.remaining, 1);
    }

    #[test]
    fn check_acceptance_combines_with_other_field_updates() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(Some(ACCEPTANCE_FIXTURE));
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            check_acceptance: vec!["2".to_string()],
            priority: Some("1".to_string()),
            add_label: vec!["reviewed".to_string()],
            ..Default::default()
        };

        let output = run_acceptance_update(&beads_dir, &args).expect("combined update");

        let after = acceptance_field(&beads_dir);
        assert_eq!(after.priority, Priority(1));
        let labels = config::open_storage_with_cli(&beads_dir, &CliOverrides::default())
            .expect("storage")
            .storage
            .get_labels("bd-ac")
            .expect("labels");
        assert!(labels.contains(&"reviewed".to_string()), "{labels:?}");
        assert!(
            after
                .acceptance_criteria
                .as_deref()
                .unwrap()
                .contains("- [x] rollback path exercised\n")
        );
        assert_eq!(output.updated_issues[0].priority, 1);
    }

    /// GitHub #467: `br update` must refuse to silently replace a non-empty
    /// accumulating text field with different content unless `--force`.
    #[test]
    fn test_text_field_overwrite_guard() {
        init_test_logging();
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-guard".to_string(),
            title: "guard test".to_string(),
            issue_type: IssueType::Task,
            priority: Priority::MEDIUM,
            description: Some("line one\nline two\nline three".to_string()),
            ..Default::default()
        };
        storage.create_issue(&issue, "tester").unwrap();
        let ids = vec!["bd-guard".to_string()];

        // Non-empty -> different content: refused without force.
        let overwrite = IssueUpdate {
            description: Some(Some("1".to_string())),
            ..Default::default()
        };
        let err =
            validate_text_field_overwrite_guard(&storage, &ids, &overwrite, false).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("description"), "{message}");
        assert!(message.contains("--force"), "{message}");

        // Non-empty -> empty (unset shell variable case): refused too.
        let clear = IssueUpdate {
            description: Some(Some(String::new())),
            ..Default::default()
        };
        assert!(validate_text_field_overwrite_guard(&storage, &ids, &clear, false).is_err());

        // Same value: silent no-op.
        let same = IssueUpdate {
            description: Some(Some("line one\nline two\nline three".to_string())),
            ..Default::default()
        };
        validate_text_field_overwrite_guard(&storage, &ids, &same, false).unwrap();

        // Empty field -> value: always allowed.
        let fill = IssueUpdate {
            notes: Some(Some("fresh notes".to_string())),
            ..Default::default()
        };
        validate_text_field_overwrite_guard(&storage, &ids, &fill, false).unwrap();

        // Force overrides the refusal.
        validate_text_field_overwrite_guard(&storage, &ids, &overwrite, true).unwrap();

        // Untouched fields (status/priority-only updates) never trip it.
        let status_only = IssueUpdate {
            status: Some(Status::InProgress),
            ..Default::default()
        };
        validate_text_field_overwrite_guard(&storage, &ids, &status_only, false).unwrap();
    }

    // === Magnitude-tiered overwrite guard (GitHub #481) ===

    const GUARD_PROSE: &str = "The importer must keep every comment id stable across a merge, \
because downstream tooling links to them.\n\n- [ ] schema migration applied\n\
- [ ] rollback path exercised\n- [ ] telemetry counter emitted\n";

    #[test]
    fn overwrite_tiers_are_proportional_to_what_is_lost() {
        init_test_logging();
        // Identical / empty current: safe.
        assert_eq!(
            classify_text_field_overwrite(GUARD_PROSE, GUARD_PROSE),
            OverwriteTier::Safe
        );
        assert_eq!(
            classify_text_field_overwrite("", "anything"),
            OverwriteTier::Safe
        );

        // Ticking four boxes in place (the #481 worked example): a revision.
        let ticked = GUARD_PROSE.replace("- [ ]", "- [x]");
        assert_eq!(
            classify_text_field_overwrite(GUARD_PROSE, &ticked),
            OverwriteTier::Revision
        );
        // Appending a paragraph: growth is a revision too.
        let grown = format!("{GUARD_PROSE}\nAlso verify the WAL is truncated afterwards.\n");
        assert_eq!(
            classify_text_field_overwrite(GUARD_PROSE, &grown),
            OverwriteTier::Revision
        );

        // Truncation to a fragment (bad heredoc / truncated read): destructive.
        let current_chars = GUARD_PROSE.chars().count();
        match classify_text_field_overwrite(GUARD_PROSE, "The importer must") {
            OverwriteTier::Destructive {
                current_chars: reported,
                incoming_chars,
            } => {
                assert_eq!(reported, current_chars);
                assert_eq!(incoming_chars, 17);
            }
            other => panic!("expected destructive tier, got {other:?}"),
        }
        // Clearing: destructive.
        assert!(matches!(
            classify_text_field_overwrite(GUARD_PROSE, ""),
            OverwriteTier::Destructive {
                incoming_chars: 0,
                ..
            }
        ));

        // Comparable size but almost none of the words kept (wrong variable):
        // allowed with an advisory.
        let unrelated: String = "lorem ipsum dolor sit amet consectetur adipiscing elit "
            .repeat(4)
            .chars()
            .take(current_chars)
            .collect();
        match classify_text_field_overwrite(GUARD_PROSE, &unrelated) {
            OverwriteTier::Rewrite {
                retained_words,
                total_words,
                ..
            } => {
                assert_eq!(retained_words, 0);
                assert!(total_words > 10);
            }
            other => panic!("expected rewrite tier, got {other:?}"),
        }
    }

    #[test]
    fn overwrite_length_boundary_is_exactly_half() {
        init_test_logging();
        // 20-char field; both halves are unrelated words so only length decides
        // between destructive and the (advisory) rewrite tier.
        let current = "aaaa bbbb cccc dddd ";
        assert_eq!(current.chars().count(), 20);
        assert!(matches!(
            classify_text_field_overwrite(current, "xxxx yyyy "), // 10 chars: exactly half
            OverwriteTier::Rewrite { .. }
        ));
        assert!(matches!(
            classify_text_field_overwrite(current, "xxxx yyyy"), // 9 chars: below half
            OverwriteTier::Destructive { .. }
        ));
        // Retention boundary: exactly half the words kept is a revision.
        assert_eq!(
            classify_text_field_overwrite(current, "aaaa bbbb xxxx yyyy "),
            OverwriteTier::Revision
        );
        assert!(matches!(
            classify_text_field_overwrite(current, "aaaa xxxx yyyy zzzz "),
            OverwriteTier::Rewrite { .. }
        ));
        // Multiset semantics: a repeated word only counts as retained as many
        // times as the incoming text contains it.
        assert_eq!(retained_word_count("a a a b", "a b c"), (2, 4));
        // Chars, not bytes: multibyte content is measured the same way.
        assert_eq!(
            classify_text_field_overwrite("ééééééééé", "éééé"),
            OverwriteTier::Destructive {
                current_chars: 9,
                incoming_chars: 4
            }
        );
    }

    #[test]
    fn overwrite_guard_passes_revisions_and_reports_rewrites() {
        init_test_logging();
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-tier".to_string(),
            title: "tier test".to_string(),
            issue_type: IssueType::Task,
            priority: Priority::MEDIUM,
            description: Some(GUARD_PROSE.to_string()),
            ..Default::default()
        };
        storage.create_issue(&issue, "tester").unwrap();
        let ids = vec!["bd-tier".to_string()];

        // Revision: no --force, no advisory.
        let revision = IssueUpdate {
            description: Some(Some(GUARD_PROSE.replace("- [ ]", "- [x]"))),
            ..Default::default()
        };
        assert!(
            validate_text_field_overwrite_guard(&storage, &ids, &revision, false)
                .unwrap()
                .is_empty()
        );

        // Rewrite of comparable size: passes with one advisory naming lengths.
        let rewrite = IssueUpdate {
            description: Some(Some(
                "completely different prose of roughly the same overall length as before, \
                 written from scratch so that hardly any of the original words remain here \
                 at all, which is exactly the shape a wrong shell variable produces"
                    .to_string(),
            )),
            ..Default::default()
        };
        let advisories =
            validate_text_field_overwrite_guard(&storage, &ids, &rewrite, false).unwrap();
        assert_eq!(advisories.len(), 1, "{advisories:?}");
        assert!(advisories[0].contains("bd-tier: replacing 'description'"));
        assert!(advisories[0].contains("words"), "{}", advisories[0]);

        // Severe shrink: refused, and the message states the ratio.
        let truncated = IssueUpdate {
            description: Some(Some("The importer must keep every".to_string())),
            ..Default::default()
        };
        let err =
            validate_text_field_overwrite_guard(&storage, &ids, &truncated, false).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("% of the current length"), "{message}");
        assert!(message.contains("--force"), "{message}");
        // --force still overrides.
        validate_text_field_overwrite_guard(&storage, &ids, &truncated, true).unwrap();
    }

    // === --append-notes (GitHub #480) ===

    #[test]
    fn append_notes_join_preserves_existing_bytes() {
        init_test_logging();
        assert_eq!(append_notes_body("", "first"), "first");
        assert_eq!(append_notes_body("one", "two"), "one\ntwo");
        assert_eq!(append_notes_body("one\n", "two"), "one\ntwo");
        assert_eq!(append_notes_body("one\n\n", "two"), "one\n\ntwo");
        assert_eq!(
            append_notes_body("  padded  \r\n", "two"),
            "  padded  \r\ntwo"
        );

        assert!(notes_already_end_with("two", "two"));
        assert!(notes_already_end_with("one\ntwo", "two"));
        assert!(notes_already_end_with("one\ntwo\nthree", "two\nthree"));
        assert!(!notes_already_end_with("onetwo", "two"));
        assert!(!notes_already_end_with("one\ntwo\n", "two"));
        assert!(!notes_already_end_with("", "two"));
    }

    fn notes_field(beads_dir: &Path) -> Option<String> {
        acceptance_field(beads_dir).notes
    }

    #[test]
    fn append_notes_creates_and_extends_the_field_without_force() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(None);
        // Empty field: created from the appended text.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec!["constraint: must run under 5s".to_string()],
            ..Default::default()
        };
        let output = run_acceptance_update(&beads_dir, &args).expect("append into empty field");
        assert_eq!(
            notes_field(&beads_dir).as_deref(),
            Some("constraint: must run under 5s")
        );
        let state = output.updated_issues[0]
            .notes
            .as_ref()
            .expect("notes state in machine output");
        assert!(state.applied);
        assert_eq!(state.chars_before, 0);
        assert_eq!(state.chars_after, 29);
        assert_eq!(state.text, "constraint: must run under 5s");

        // Populated field: two values land as two lines after the existing
        // content, which is untouched; description survives; no --force.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec![
                "decision: keep the WAL".to_string(),
                "-starts with a dash".to_string(),
            ],
            priority: Some("1".to_string()),
            ..Default::default()
        };
        let output = run_acceptance_update(&beads_dir, &args).expect("append twice");
        assert_eq!(
            notes_field(&beads_dir).as_deref(),
            Some("constraint: must run under 5s\ndecision: keep the WAL\n-starts with a dash")
        );
        let after = acceptance_field(&beads_dir);
        assert_eq!(after.priority, Priority::HIGH);
        assert_eq!(
            after.description.as_deref(),
            Some("context that must survive")
        );
        let json = serde_json::to_value(&output.updated_issues[0]).unwrap();
        assert_eq!(json["notes"]["applied"], true);
        assert_eq!(
            json["notes"]["appended"],
            "decision: keep the WAL\n-starts with a dash"
        );
        assert_eq!(json["notes"]["chars_before"], 29);
        assert_eq!(
            json["notes"]["text"],
            "constraint: must run under 5s\ndecision: keep the WAL\n-starts with a dash"
        );
        assert!(
            json.get("acceptance_criteria").is_none(),
            "acceptance state must not appear without acceptance flags"
        );
    }

    #[test]
    fn append_notes_rerun_is_a_no_op_and_empty_text_is_rejected() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(None);
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec!["decision: keep the WAL".to_string()],
            ..Default::default()
        };
        run_acceptance_update(&beads_dir, &args).expect("first append");
        let before = acceptance_field(&beads_dir);

        let output = run_acceptance_update(&beads_dir, &args).expect("idempotent re-run");
        let after = acceptance_field(&beads_dir);
        assert_eq!(after.notes, before.notes);
        assert_eq!(
            after.updated_at, before.updated_at,
            "a skipped append must not rewrite the row"
        );
        let state = output.updated_issues[0].notes.as_ref().unwrap();
        assert!(!state.applied);
        assert_eq!(state.chars_before, state.chars_after);

        // A same-length identical value combined with the whole-field flag
        // is refused at the parser; here, blank text is refused before any
        // write.
        let blank = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec!["   ".to_string()],
            ..Default::default()
        };
        let err = run_acceptance_update(&beads_dir, &blank).unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
        assert_eq!(acceptance_field(&beads_dir).notes, before.notes);
    }

    #[test]
    fn append_notes_never_trips_the_overwrite_guard() {
        init_test_logging();
        let (_temp, beads_dir) = acceptance_workspace(None);
        let seed = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            notes: Some(GUARD_PROSE.to_string()),
            ..Default::default()
        };
        run_acceptance_update(&beads_dir, &seed).expect("fill empty notes");

        // Appending a short line to a long field is exactly the shape the
        // whole-field guard refuses; the append path must not consult it.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec!["ok".to_string()],
            ..Default::default()
        };
        run_acceptance_update(&beads_dir, &args).expect("append without --force");
        assert_eq!(notes_field(&beads_dir), Some(format!("{GUARD_PROSE}ok")));

        // Bulk label route stays disabled when an append rides along.
        let args = UpdateArgs {
            ids: vec!["bd-ac".to_string()],
            append_notes: vec!["tagged".to_string()],
            add_label: vec!["backend".to_string()],
            ..Default::default()
        };
        run_acceptance_update(&beads_dir, &args).expect("append with label");
        let after = acceptance_field(&beads_dir);
        assert_eq!(after.notes, Some(format!("{GUARD_PROSE}ok\ntagged")));
        assert_eq!(after.labels, vec!["backend".to_string()]);
    }

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
    fn test_resolve_update_description_file_preserves_exact_content() {
        init_test_logging();
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("description.md");
        let exact = "  leading whitespace\n\n# Markdown\n\ntrailing newline\n";
        fs::write(&path, exact).unwrap();
        let args = UpdateArgs {
            description_file: Some(path),
            ..Default::default()
        };

        let resolved = resolve_update_description(&args).unwrap();

        assert_eq!(resolved.description.as_deref(), Some(exact));
        assert!(resolved.description_file.is_none());
    }

    #[test]
    fn test_resolve_update_description_rejects_programmatic_conflict_before_read() {
        init_test_logging();
        let args = UpdateArgs {
            description: Some("inline".to_string()),
            description_file: Some(Path::new("/definitely/missing/description.md").to_path_buf()),
            ..Default::default()
        };

        let err = resolve_update_description(&args).unwrap_err();

        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn test_build_update_with_status() {
        init_test_logging();
        info!("test_build_update_with_status: starting");
        // Non-terminal status transitions still flow through build_update.
        // Terminal transitions (closed/tombstone) are rejected up-front by
        // `reject_terminal_status_transition` — see beads_rust#301 and the
        // dedicated tests below.
        let args_blocked = UpdateArgs {
            status: Some("blocked".to_string()),
            ..Default::default()
        };
        let update_blocked = build_update(&args_blocked, "test_actor", false).unwrap();
        assert_eq!(update_blocked.status, Some(Status::Blocked));
        // Close metadata should be explicitly cleared for non-terminal statuses.
        assert_eq!(update_blocked.closed_at, Some(None));
        assert_eq!(update_blocked.close_reason, Some(None));
        assert_eq!(update_blocked.closed_by_session, Some(None));

        let args_in_progress = UpdateArgs {
            status: Some("in_progress".to_string()),
            ..Default::default()
        };
        let update_in_progress = build_update(&args_in_progress, "test_actor", false).unwrap();
        assert_eq!(update_in_progress.status, Some(Status::InProgress));
        info!("test_build_update_with_status: assertions passed");
    }

    /// beads_rust#301: `br update --status closed` must refuse and direct
    /// the operator at `br close` so close-policy fires.
    #[test]
    fn reject_terminal_status_transition_refuses_closed() {
        init_test_logging();
        let err = reject_terminal_status_transition(Some("closed")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("br close"),
            "error must point at br close; got: {msg}"
        );
        assert!(
            msg.contains("close-policy"),
            "error must mention close-policy; got: {msg}"
        );
        assert!(
            msg.contains("#301") || msg.contains("issues/301"),
            "error must link the originating issue; got: {msg}"
        );
    }

    /// beads_rust#301: tombstone is also a terminal state with a dedicated
    /// command (`br delete`); refuse the update path so dependency rewiring
    /// is not skipped.
    #[test]
    fn reject_terminal_status_transition_refuses_tombstone() {
        init_test_logging();
        let err = reject_terminal_status_transition(Some("tombstone")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("br delete"),
            "error must point at br delete; got: {msg}"
        );
    }

    /// Non-terminal statuses (open/in_progress/blocked/deferred/draft) and
    /// the absence of `--status` must keep working unchanged — the rejection
    /// is scoped to terminal states only.
    #[test]
    fn reject_terminal_status_transition_allows_non_terminal_and_absent() {
        init_test_logging();
        reject_terminal_status_transition(None).expect("no --status must pass through");
        for ok in &[
            "open",
            "in_progress",
            "inprogress",
            "blocked",
            "deferred",
            "draft",
            "pinned",
        ] {
            let result = reject_terminal_status_transition(Some(ok));
            assert!(
                result.is_ok(),
                "status {ok} must be accepted; got {:?}",
                result.err()
            );
        }
    }

    /// Status comparison is case-insensitive and matches known aliases —
    /// neither `CLOSED` nor `Closed` should sneak past the gate.
    #[test]
    fn reject_terminal_status_transition_is_case_insensitive() {
        init_test_logging();
        for terminal in &["Closed", "CLOSED", "Tombstone", "TOMBSTONE"] {
            let result = reject_terminal_status_transition(Some(terminal));
            assert!(result.is_err(), "status {terminal} must be rejected");
            let err = result.unwrap_err();
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn test_build_update_rejects_session_without_closing() {
        let args = UpdateArgs {
            session: Some("session-123".to_string()),
            ..Default::default()
        };
        let err = build_update(&args, "test_actor", false).unwrap_err();
        assert!(err.to_string().contains("--session can only be used"));

        let args_open = UpdateArgs {
            status: Some("open".to_string()),
            session: Some("session-123".to_string()),
            ..Default::default()
        };
        let err = build_update(&args_open, "test_actor", false).unwrap_err();
        assert!(err.to_string().contains("--session can only be used"));
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

    #[test]
    fn test_update_output_partition_matches_previous_mode_checks() {
        let cases = [
            (OutputMode::Json, true, false),
            (OutputMode::Toon, true, false),
            (OutputMode::Quiet, false, false),
            (OutputMode::Rich, false, true),
            (OutputMode::Plain, false, true),
        ];

        for (mode, expected_machine, expected_human) in cases {
            let ctx = OutputContext::with_mode(mode);

            assert_eq!(update_uses_machine_output(&ctx), expected_machine);
            assert_eq!(update_uses_human_output(&ctx), expected_human);
        }
    }

    #[test]
    fn update_human_lines_sanitize_issue_ids_and_titles() {
        let updated = updated_issue_human_line("bd-1\x1b]52;c;bad\x07", "Title\x1b[2J\nnext");
        let no_updates = no_updates_human_line("bd-2\x07");

        assert!(!updated.contains('\x1b'));
        assert!(!updated.contains('\x07'));
        assert!(!no_updates.contains('\x07'));
        assert_eq!(
            updated,
            "Updated bd-1\\u{1b}]52;c;bad\\u{7}: Title\\u{1b}[2J\\nnext"
        );
        assert_eq!(no_updates, "No updates specified for bd-2\\u{7}");
    }

    #[test]
    fn reorder_routed_items_sanitizes_missing_input_error() {
        let requested = vec!["bd-update\x1b[2J\nbad".to_string(), "bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-ok".to_string()], vec!["ok"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "update routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = err.to_string();
        assert!(!message.chars().any(char::is_control));
        assert!(message.contains("\\u{1b}[2J"));
        assert!(message.contains("\\n"));
    }

    #[test]
    fn reorder_routed_items_sanitizes_unexpected_input_error() {
        let requested = vec!["bd-ok".to_string()];
        let routed_items = vec![(vec!["bd-update\x1b[2J\nbad".to_string()], vec!["bad"])];

        let err =
            reorder_routed_items_by_requested_inputs(&requested, routed_items, "update routing")
                .unwrap_err();

        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let message = err.to_string();
        assert!(!message.chars().any(char::is_control));
        assert!(message.contains("\\u{1b}[2J"));
        assert!(message.contains("\\n"));
    }

    #[test]
    fn test_validate_mutable_target_issues_rejects_tombstone() {
        init_test_logging();
        info!("test_validate_mutable_target_issues_rejects_tombstone: starting");

        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-tombstone".to_string(),
            title: "Deleted issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .delete_issue("bd-tombstone", "tester", "delete for update test", None)
            .unwrap();

        let err = validate_mutable_target_issues(&storage, &["bd-tombstone".to_string()], true)
            .unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { .. }),
            "unexpected error: {err:?}"
        );
        let BeadsError::Validation { field, reason } = err else {
            return;
        };
        assert_eq!(field, "issue");
        assert!(reason.contains("cannot update tombstone issue"));

        info!("test_validate_mutable_target_issues_rejects_tombstone: assertions passed");
    }

    #[test]
    fn test_validate_mutable_target_issues_allows_open_issue() {
        init_test_logging();
        info!("test_validate_mutable_target_issues_allows_open_issue: starting");

        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-open".to_string(),
            title: "Open issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };
        storage.create_issue(&issue, "tester").unwrap();

        validate_mutable_target_issues(&storage, &["bd-open".to_string()], true).unwrap();

        info!("test_validate_mutable_target_issues_allows_open_issue: assertions passed");
    }

    #[test]
    fn test_validate_route_runtime_guards_rejects_assigned_claim_target() {
        init_test_logging();
        info!("test_validate_route_runtime_guards_rejects_assigned_claim_target: starting");

        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-claimed".to_string(),
            title: "Claimed issue".to_string(),
            assignee: Some("bob".to_string()),
            status: Status::InProgress,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };
        storage.create_issue(&issue, "tester").unwrap();

        let update = IssueUpdate {
            expect_unassigned: true,
            claim_actor: Some("alice".to_string()),
            assignee: Some(Some("alice".to_string())),
            status: Some(Status::InProgress),
            ..IssueUpdate::default()
        };

        let err = validate_route_runtime_guards(&storage, &["bd-claimed".to_string()], &update)
            .unwrap_err();
        assert!(err.to_string().contains("already assigned to bob"));

        info!(
            "test_validate_route_runtime_guards_rejects_assigned_claim_target: assertions passed"
        );
    }

    #[test]
    fn test_validate_multi_issue_external_ref_update_rejects_multiple_distinct_ids() {
        init_test_logging();
        info!(
            "test_validate_multi_issue_external_ref_update_rejects_multiple_distinct_ids: starting"
        );

        let err = validate_multi_issue_external_ref_update(
            Some("EXT-123"),
            &["bd-1".to_string(), "bd-2".to_string()],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot set external_ref 'EXT-123'")
        );

        info!(
            "test_validate_multi_issue_external_ref_update_rejects_multiple_distinct_ids: assertions passed"
        );
    }

    #[test]
    fn test_prepare_single_route_rejects_invalid_remove_label() {
        init_test_logging();
        info!("test_prepare_single_route_rejects_invalid_remove_label: starting");

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        {
            let mut storage_ctx =
                config::open_storage_with_cli(&beads_dir, &CliOverrides::default())
                    .expect("storage");
            let issue = Issue {
                id: "bd-label".to_string(),
                title: "Label target".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                issue_type: IssueType::Task,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                ..Issue::default()
            };
            storage_ctx
                .storage
                .create_issue(&issue, "tester")
                .expect("create issue");
        }

        let args = UpdateArgs {
            ids: vec!["bd-label".to_string()],
            remove_label: vec!["has space".to_string()],
            ..Default::default()
        };
        let result = prepare_single_route(&args, &CliOverrides::default(), &beads_dir, false);
        assert!(result.is_err(), "invalid remove label should fail");
        if let Err(err) = result {
            assert!(err.to_string().contains("invalid characters"));
        }
        info!("test_prepare_single_route_rejects_invalid_remove_label: assertions passed");
    }

    #[test]
    fn test_execute_prepared_route_bulk_label_add_updates_multiple_ids() {
        init_test_logging();
        info!("test_execute_prepared_route_bulk_label_add_updates_multiple_ids: starting");

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let mut storage_ctx =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        for id in ["bd-bulk-a", "bd-bulk-b"] {
            let issue = Issue {
                id: id.to_string(),
                title: format!("Bulk target {id}"),
                status: Status::Open,
                priority: Priority::MEDIUM,
                issue_type: IssueType::Task,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                ..Issue::default()
            };
            storage_ctx
                .storage
                .create_issue(&issue, "tester")
                .expect("create issue");
        }
        storage_ctx
            .storage
            .clear_all_dirty_issues()
            .expect("clear dirty state");

        let prepared = PreparedUpdateRoute {
            storage_ctx,
            actor: "tester".to_string(),
            resolved_ids: vec!["bd-bulk-a".to_string(), "bd-bulk-b".to_string()],
            update: IssueUpdate::default(),
            has_updates: true,
            add_labels: vec!["bulk-route".to_string()],
            remove_labels: Vec::new(),
            set_labels: false,
            valid_set_labels: Vec::new(),
            resolved_parent: ParentUpdatePlan::Unchanged,
            acceptance_edits: Vec::new(),
            notes_appends: Vec::new(),
            overwrite_advisories: Vec::new(),
            auto_flush_external: false,
            attribution: EventAttribution::default(),
            _routed_write_lock: RoutedWorkspaceWriteLock::local(),
        };

        let ctx = OutputContext::from_flags(true, false, true);
        let output = execute_prepared_route(prepared, &ctx).expect("bulk route update");
        assert_eq!(output.updated_issues.len(), 2);
        assert_eq!(
            output.resolved_ids,
            vec!["bd-bulk-a".to_string(), "bd-bulk-b".to_string()]
        );

        let reopened =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("reopen");
        assert_eq!(
            reopened.storage.get_labels("bd-bulk-a").expect("labels a"),
            vec!["bulk-route".to_string()]
        );
        assert_eq!(
            reopened.storage.get_labels("bd-bulk-b").expect("labels b"),
            vec!["bulk-route".to_string()]
        );
        let mut dirty = reopened.storage.get_dirty_issue_ids().expect("dirty ids");
        dirty.sort();
        assert_eq!(
            dirty,
            vec!["bd-bulk-a".to_string(), "bd-bulk-b".to_string()]
        );

        info!("test_execute_prepared_route_bulk_label_add_updates_multiple_ids: assertions passed");
    }

    #[test]
    fn test_execute_prepared_route_bulk_label_remove_updates_multiple_ids() {
        init_test_logging();
        info!("test_execute_prepared_route_bulk_label_remove_updates_multiple_ids: starting");

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let mut storage_ctx =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        for id in ["bd-bulk-remove-a", "bd-bulk-remove-b"] {
            let issue = Issue {
                id: id.to_string(),
                title: format!("Bulk remove target {id}"),
                status: Status::Open,
                priority: Priority::MEDIUM,
                issue_type: IssueType::Task,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                ..Issue::default()
            };
            storage_ctx
                .storage
                .create_issue(&issue, "tester")
                .expect("create issue");
            storage_ctx
                .storage
                .add_label(id, "bulk-route-remove", "tester")
                .expect("add label");
        }
        storage_ctx
            .storage
            .clear_all_dirty_issues()
            .expect("clear dirty state");

        let prepared = PreparedUpdateRoute {
            storage_ctx,
            actor: "tester".to_string(),
            resolved_ids: vec![
                "bd-bulk-remove-a".to_string(),
                "bd-bulk-remove-b".to_string(),
            ],
            update: IssueUpdate::default(),
            has_updates: true,
            add_labels: Vec::new(),
            remove_labels: vec!["bulk-route-remove".to_string()],
            set_labels: false,
            valid_set_labels: Vec::new(),
            resolved_parent: ParentUpdatePlan::Unchanged,
            acceptance_edits: Vec::new(),
            notes_appends: Vec::new(),
            overwrite_advisories: Vec::new(),
            auto_flush_external: false,
            attribution: EventAttribution::default(),
            _routed_write_lock: RoutedWorkspaceWriteLock::local(),
        };

        let ctx = OutputContext::from_flags(true, false, true);
        let output = execute_prepared_route(prepared, &ctx).expect("bulk route update");
        assert_eq!(output.updated_issues.len(), 2);
        assert_eq!(
            output.resolved_ids,
            vec![
                "bd-bulk-remove-a".to_string(),
                "bd-bulk-remove-b".to_string()
            ]
        );

        let reopened =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("reopen");
        assert!(
            reopened
                .storage
                .get_labels("bd-bulk-remove-a")
                .expect("labels a")
                .is_empty()
        );
        assert!(
            reopened
                .storage
                .get_labels("bd-bulk-remove-b")
                .expect("labels b")
                .is_empty()
        );
        let mut dirty = reopened.storage.get_dirty_issue_ids().expect("dirty ids");
        dirty.sort();
        assert_eq!(
            dirty,
            vec![
                "bd-bulk-remove-a".to_string(),
                "bd-bulk-remove-b".to_string()
            ]
        );

        info!(
            "test_execute_prepared_route_bulk_label_remove_updates_multiple_ids: assertions passed"
        );
    }

    #[test]
    fn test_execute_prepared_route_repairs_blocked_cache_after_late_update_error() {
        init_test_logging();
        info!(
            "test_execute_prepared_route_repairs_blocked_cache_after_late_update_error: starting"
        );

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let mut storage_ctx =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");

        let blocker = Issue {
            id: "bd-blocker".to_string(),
            title: "Blocker".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };
        let dependent = Issue {
            id: "bd-blocked".to_string(),
            title: "Blocked".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ..Issue::default()
        };

        storage_ctx
            .storage
            .create_issue(&blocker, "tester")
            .expect("create blocker");
        storage_ctx
            .storage
            .create_issue(&dependent, "tester")
            .expect("create blocked");
        storage_ctx
            .storage
            .add_dependency("bd-blocked", "bd-blocker", "blocks", "tester")
            .expect("create dependency");

        assert!(
            storage_ctx
                .storage
                .get_blocked_ids()
                .expect("blocked ids before update")
                .contains("bd-blocked")
        );

        storage_ctx
            .storage
            .execute_raw("DROP TABLE labels")
            .expect("drop labels table");

        let prepared = PreparedUpdateRoute {
            storage_ctx,
            actor: "tester".to_string(),
            resolved_ids: vec!["bd-blocker".to_string()],
            update: IssueUpdate {
                status: Some(Status::Closed),
                ..IssueUpdate::default()
            },
            has_updates: true,
            add_labels: vec!["late-runtime-error".to_string()],
            remove_labels: Vec::new(),
            set_labels: false,
            valid_set_labels: Vec::new(),
            resolved_parent: ParentUpdatePlan::Unchanged,
            acceptance_edits: Vec::new(),
            notes_appends: Vec::new(),
            overwrite_advisories: Vec::new(),
            auto_flush_external: false,
            attribution: EventAttribution::default(),
            _routed_write_lock: RoutedWorkspaceWriteLock::local(),
        };

        let ctx = OutputContext::from_flags(false, false, true);
        let err = execute_prepared_route(prepared, &ctx).expect_err("update should fail");
        assert!(
            !err.to_string().contains("failed to rebuild blocked cache"),
            "late runtime error should not be masked by blocked-cache repair: {err}"
        );

        let reopened =
            config::open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("reopen");
        let blocker_after = reopened
            .storage
            .get_issue("bd-blocker")
            .expect("load blocker")
            .expect("blocker should still exist");
        assert_eq!(blocker_after.status, Status::Closed);
        assert!(
            !reopened
                .storage
                .get_blocked_ids()
                .expect("blocked ids after repair")
                .contains("bd-blocked"),
            "dependent issue should be unblocked after the blocker closed despite the later error"
        );

        info!(
            "test_execute_prepared_route_repairs_blocked_cache_after_late_update_error: assertions passed"
        );
    }
}
