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
use crate::validation::text_guard::{TextChange, TextField};
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
    /// Before/after sizes for every free-text field this command wrote.
    ///
    /// Emitted so an agent parsing JSON sees the same delta a human sees on
    /// the success line; agents never read the human line.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    text_deltas: Vec<TextDeltaOutput>,
}

/// JSON shape for one free-text field's before/after sizes.
#[derive(Serialize)]
struct TextDeltaOutput {
    field: &'static str,
    old_chars: usize,
    new_chars: usize,
    /// Whether the previous value survives verbatim inside the new one.
    ///
    /// Absent when there was no prior content to retain. `false` on a write
    /// that GREW the field means the growth still dropped everything that was
    /// there — the read-modify-write-with-a-failed-preimage case, which the
    /// shrink guard cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_content_retained: Option<bool>,
    /// Size of the value bd was ASKED to write.
    ///
    /// Present only when it differs from `new_chars`, i.e. only when the write
    /// did not land as sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_chars: Option<usize>,
    /// Whether what is stored is what bd was asked to store.
    ///
    /// Emitted only when it is `false`, so its mere presence is the alarm.
    #[serde(skip_serializing_if = "Option::is_none")]
    landed_as_sent: Option<bool>,
}

impl From<&TextChange> for TextDeltaOutput {
    fn from(change: &TextChange) -> Self {
        let mismatch = change.landed_as_sent == Some(false);
        Self {
            field: change.field.name(),
            old_chars: change.old_chars,
            new_chars: change.new_chars,
            // Suppressed when the write did not land as sent, exactly as on
            // the human line: "prior content retained" is a true answer to a
            // question the caller has stopped caring about, and next to a
            // failure it reads as reassurance.
            prior_content_retained: if mismatch {
                None
            } else {
                change.prior_retained
            },
            requested_chars: if mismatch {
                change.requested_chars
            } else {
                None
            },
            landed_as_sent: if mismatch { Some(false) } else { None },
        }
    }
}

impl UpdatedIssueOutput {
    fn new(issue: &Issue, text_deltas: Vec<TextDeltaOutput>) -> Self {
        Self {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status.as_str().to_string(),
            priority: issue.priority.0,
            updated_at: issue.updated_at,
            text_deltas,
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

    let actor = config::resolve_actor_with_storage(&config_layer, &storage_ctx.storage);
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

    // Free-text writes are checked against the STORED value before anything
    // is mutated, for every target id. bd is the only participant that holds
    // both the old value and the incoming one without a subprocess, so the
    // read happens here rather than being taken on trust from the caller.
    let text_writes = requested_text_writes(args);
    refuse_destructive_shrinks(
        &storage_ctx.storage,
        &resolved_ids,
        &text_writes,
        args.replace,
    )?;

    let claim_exclusive = config::claim_exclusive_from_layer(&config_layer);
    let update = build_update(args, &actor, claim_exclusive)?;
    let has_updates = !update.is_empty()
        || !args.add_label.is_empty()
        || !args.remove_label.is_empty()
        || !args.set_labels.is_empty()
        || args.parent.is_some();

    let mut updated_issues: Vec<UpdatedIssueOutput> = Vec::new();
    let mut write_mismatch: Option<BeadsError> = None;

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
        if let (Some(issue_before), Some(target_status)) = (&issue_before, &update.status) {
            if issue_before.status == *target_status {
                warn_redundant_status(id, target_status, storage);
            }
        }

        // Apply basic field updates
        if !update.is_empty() {
            storage.update_issue(id, &update, &actor)?;
        }

        apply_label_updates(storage, id, args, &actor)?;

        // Apply parent
        apply_parent_update(storage, id, args.parent.as_deref(), &resolver, &actor)?;

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        // Get issue after update for output
        let issue_after = storage.get_issue(id)?;

        if let Some(issue) = issue_after {
            let text_changes =
                measure_text_changes(id, &text_writes, issue_before.as_ref(), &issue);
            // Remember the FIRST write that did not land as sent; it is raised
            // after every issue has reported, so the caller sees what happened
            // to each field and then exits non-zero.
            if write_mismatch.is_none() {
                write_mismatch = first_write_mismatch(id, &text_changes);
            }
            report_one_update(
                id,
                issue_before.as_ref(),
                &issue,
                &text_changes,
                has_updates,
                ctx,
                &mut updated_issues,
            );
        }
    }

    if ctx.is_json() {
        ctx.json_pretty(&updated_issues);
    }

    storage_ctx.flush_no_db_if_dirty()?;

    // A write that did not land as sent has already happened and cannot be
    // refused, but a caller running `br update ... && next-step` must not
    // proceed as though it applied. Distinct exit code (2) from the
    // DESTRUCTIVE_UPDATE refusal (4) so automation can tell the two apart.
    if let Some(mismatch) = write_mismatch {
        return Err(mismatch);
    }

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

/// The free-text field values this invocation asks to write, in CLI order.
///
/// Read from `args` rather than from the assembled `IssueUpdate` so the guard
/// sees exactly what the caller passed, including an explicit empty string.
fn requested_text_writes(args: &UpdateArgs) -> Vec<(TextField, &str)> {
    let candidates = [
        (TextField::Title, args.title.as_deref()),
        (TextField::Description, args.description.as_deref()),
        (TextField::Design, args.design.as_deref()),
        (
            TextField::AcceptanceCriteria,
            args.acceptance_criteria.as_deref(),
        ),
        (TextField::Notes, args.notes.as_deref()),
    ];
    candidates
        .into_iter()
        .filter_map(|(field, value)| value.map(|v| (field, v)))
        .collect()
}

/// The currently stored value of a free-text field ("" when unset).
fn stored_text(issue: &Issue, field: TextField) -> &str {
    match field {
        TextField::Title => issue.title.as_str(),
        TextField::Description => issue.description.as_deref().unwrap_or_default(),
        TextField::Design => issue.design.as_deref().unwrap_or_default(),
        TextField::AcceptanceCriteria => issue.acceptance_criteria.as_deref().unwrap_or_default(),
        TextField::Notes => issue.notes.as_deref().unwrap_or_default(),
    }
}

/// Refuse, before any write happens, any update that would shrink a
/// free-text field that currently has content.
///
/// This runs over EVERY target id up front: a multi-id update either applies
/// everywhere or nowhere, rather than destroying the first issue and then
/// reporting a refusal for the second.
///
/// The shrink test is a cheap proxy for destruction, not a proof of it — a
/// write that GROWS the field can still drop everything that was there. That
/// case is deliberately allowed and reported instead (see
/// `crate::validation::text_guard`).
///
/// # Errors
///
/// Returns `BeadsError::DestructiveFieldShrink` for the first offending
/// (issue, field) pair. Nothing has been written when it does.
fn refuse_destructive_shrinks(
    storage: &SqliteStorage,
    ids: &[String],
    writes: &[(TextField, &str)],
    replace_opt_in: bool,
) -> Result<()> {
    if writes.is_empty() || replace_opt_in {
        return Ok(());
    }

    for id in ids {
        // FAIL CLOSED. If the stored value cannot be read there is nothing to
        // compare against, and "the comparison did not happen" must never be
        // reported as "the comparison passed" — that is the exact shape of the
        // vacuous preservation checks this guard exists to replace. Ids are
        // existence-checked during resolution, so this is not reachable by an
        // ordinary caller; it stays a refusal rather than a skip anyway.
        let Some(issue) = storage.get_issue(id)? else {
            return Err(BeadsError::IssueNotFound { id: id.clone() });
        };
        for &(field, new_value) in writes {
            let change = TextChange::measure(field, stored_text(&issue, field), new_value);
            if change.is_destructive_shrink() {
                return Err(BeadsError::DestructiveFieldShrink {
                    id: issue.id.clone(),
                    field: field.name().to_string(),
                    flag: field.flag().to_string(),
                    old_chars: change.old_chars,
                    new_chars: change.new_chars,
                });
            }
        }
    }

    Ok(())
}

/// Measure what actually happened to each free-text field this command wrote.
///
/// Uses the stored before/after values rather than the arguments, so the
/// reported numbers are the numbers in the database.
fn measure_text_changes(
    id: &str,
    writes: &[(TextField, &str)],
    before: Option<&Issue>,
    after: &Issue,
) -> Vec<TextChange> {
    if writes.is_empty() {
        return Vec::new();
    }
    let Some(before) = before else {
        // Say so out loud. A silently absent delta is indistinguishable from
        // "no free-text field was written", which is the confusion this whole
        // change exists to remove.
        eprintln!(
            "warning: {id} could not be read before the write; field size deltas unavailable"
        );
        return Vec::new();
    };
    writes
        .iter()
        .map(|&(field, requested)| {
            TextChange::measure_write(
                field,
                stored_text(before, field),
                stored_text(after, field),
                requested,
            )
        })
        .collect()
}

/// Apply `--add-label` / `--remove-label` / `--set-labels` to one issue.
fn apply_label_updates(
    storage: &mut SqliteStorage,
    id: &str,
    args: &UpdateArgs,
    actor: &str,
) -> Result<()> {
    for label in &args.add_label {
        LabelValidator::validate(label).map_err(|e| BeadsError::validation("label", e.message))?;
        storage.add_label(id, label, actor)?;
    }
    for label in &args.remove_label {
        storage.remove_label(id, label, actor)?;
    }
    if !args.set_labels.is_empty() {
        // Remove all then add new
        storage.remove_all_labels(id, actor)?;
        // Join all flag values, then split by comma (handles both
        // --set-labels a,b and --set-labels a --set-labels b)
        let combined = args.set_labels.join(",");
        for label in combined.split(',') {
            let label = label.trim();
            if !label.is_empty() {
                LabelValidator::validate(label)
                    .map_err(|e| BeadsError::validation("label", e.message))?;
                storage.add_label(id, label, actor)?;
            }
        }
    }
    Ok(())
}

/// The first field whose stored value is not the value bd was asked to store.
fn first_write_mismatch(id: &str, changes: &[TextChange]) -> Option<BeadsError> {
    changes
        .iter()
        .find(|change| change.landed_as_sent == Some(false))
        .map(|change| BeadsError::WriteDidNotLandAsSent {
            id: id.to_string(),
            field: change.field.name().to_string(),
            requested_chars: change.requested_chars.unwrap_or(change.new_chars),
            stored_chars: change.new_chars,
        })
}

/// Emit one issue's result, into the JSON accumulator or straight to stdout.
fn report_one_update(
    id: &str,
    before: Option<&Issue>,
    after: &Issue,
    text_changes: &[TextChange],
    has_updates: bool,
    ctx: &OutputContext,
    json_out: &mut Vec<UpdatedIssueOutput>,
) {
    if ctx.is_json() {
        json_out.push(UpdatedIssueOutput::new(
            after,
            text_changes.iter().map(TextDeltaOutput::from).collect(),
        ));
    } else if has_updates {
        print_update_summary(id, &after.title, before, after, text_changes);
    } else {
        println!("No updates specified for {id}");
    }
}

/// Render one field's delta for the success line.
///
/// Ordering is deliberate. A discrepancy comes FIRST, ahead of the sizes and
/// ahead of any reassuring clause, because a reader scanning the line must hit
/// the alarm before the comfort. And when the write did not land as sent, the
/// retention verdict is omitted entirely: "prior content retained" is a true
/// answer to a question the caller has stopped caring about, and printing it
/// beside a failure is how a line becomes noise.
fn format_text_change(change: &TextChange) -> String {
    let name = change.field.name();
    if change.landed_as_sent == Some(false) {
        let requested = change.requested_chars.unwrap_or(change.new_chars);
        return format!(
            "{name}: WRITE DID NOT LAND AS SENT, requested {requested} chars, stored {} \
             ({} \u{2192} {} chars)",
            change.new_chars, change.old_chars, change.new_chars
        );
    }

    let mut rendered = format!(
        "{name}: {} \u{2192} {} chars",
        change.old_chars, change.new_chars
    );
    match change.prior_retained {
        Some(true) => rendered.push_str(", prior content retained"),
        Some(false) => rendered.push_str(", PRIOR CONTENT NOT RETAINED"),
        None => {}
    }
    rendered
}

/// Print a summary of what changed for the issue.
fn print_update_summary(
    id: &str,
    title: &str,
    before: Option<&Issue>,
    after: &Issue,
    text_changes: &[TextChange],
) {
    if text_changes.is_empty() {
        println!("Updated {id}: {title}");
    } else {
        // Any field whose write did not land as sent is rendered first, so a
        // multi-field update cannot bury the alarm behind two healthy deltas.
        let (alarming, ordinary): (Vec<&TextChange>, Vec<&TextChange>) = text_changes
            .iter()
            .partition(|change| change.landed_as_sent == Some(false));
        let deltas: Vec<String> = alarming
            .into_iter()
            .chain(ordinary)
            .map(format_text_change)
            .collect();
        println!("Updated {id}: {title}  ({})", deltas.join("; "));
    }

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

    // === The success-line delta clause ===

    #[test]
    fn delta_clause_reports_growth_and_retention() {
        let change = TextChange::measure_write(TextField::Notes, "abcde", "abcdefg", "abcdefg");
        assert_eq!(
            format_text_change(&change),
            "notes: 5 \u{2192} 7 chars, prior content retained"
        );
    }

    #[test]
    fn delta_clause_shouts_when_growth_dropped_prior_content() {
        let change = TextChange::measure_write(TextField::Notes, "aaaaa", "bbbbbbb", "bbbbbbb");
        assert_eq!(
            format_text_change(&change),
            "notes: 5 \u{2192} 7 chars, PRIOR CONTENT NOT RETAINED"
        );
    }

    /// The alarm leads, and the retention verdict is gone: it would be a true
    /// statement ("prior content retained") sitting next to a failure, which
    /// is how a line stops being read.
    #[test]
    fn delta_clause_leads_with_a_write_that_did_not_land() {
        let change =
            TextChange::measure_write(TextField::Notes, "abcde", "abcde", "abcdefghijKLMNOP");
        let rendered = format_text_change(&change);
        assert_eq!(
            rendered,
            "notes: WRITE DID NOT LAND AS SENT, requested 16 chars, stored 5 (5 \u{2192} 5 chars)"
        );
        assert!(!rendered.contains("retained"), "{rendered}");
    }

    /// The JSON must carry the same verdicts as the human line, since agents
    /// read one and never the other.
    #[test]
    fn json_delta_mirrors_the_human_verdicts() {
        let ok = TextDeltaOutput::from(&TextChange::measure_write(
            TextField::Notes,
            "abcde",
            "abcdefg",
            "abcdefg",
        ));
        assert_eq!(ok.prior_content_retained, Some(true));
        assert_eq!(ok.landed_as_sent, None, "silence means it landed");
        assert_eq!(ok.requested_chars, None);

        let bad = TextDeltaOutput::from(&TextChange::measure_write(
            TextField::Notes,
            "abcde",
            "abcde",
            "abcdefghijKLMNOP",
        ));
        assert_eq!(bad.landed_as_sent, Some(false));
        assert_eq!(bad.requested_chars, Some(16));
        assert_eq!(bad.new_chars, 5);
        assert_eq!(
            bad.prior_content_retained, None,
            "the retention verdict is suppressed alongside a failed write"
        );
    }

    // === Which fields the guard sees ===

    /// Every free-text flag on `UpdateArgs` must reach the guard. A field
    /// added later that is not wired in here is silently ungated, which is
    /// the original bug with a different flag name.
    #[test]
    fn requested_writes_cover_every_free_text_field() {
        let args = UpdateArgs {
            title: Some("t".to_string()),
            description: Some("d".to_string()),
            design: Some("de".to_string()),
            acceptance_criteria: Some("ac".to_string()),
            notes: Some("n".to_string()),
            ..Default::default()
        };
        let writes = requested_text_writes(&args);
        assert_eq!(writes.len(), TextField::ALL.len());
        for field in TextField::ALL {
            assert!(
                writes.iter().any(|&(f, _)| f == field),
                "{} is settable but never reaches the guard",
                field.name()
            );
        }
    }

    /// A field the caller did not pass is not a write, and must not be
    /// measured or gated as though it were.
    #[test]
    fn unpassed_fields_are_not_writes() {
        let args = UpdateArgs {
            notes: Some("n".to_string()),
            ..Default::default()
        };
        let writes = requested_text_writes(&args);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, TextField::Notes);

        assert!(
            requested_text_writes(&UpdateArgs::default()).is_empty(),
            "a status-only update writes no free text"
        );
    }
}
