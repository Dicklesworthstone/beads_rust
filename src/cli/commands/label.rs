//! Label command implementation.
//!
//! Provides label management: add, remove, list, list-all, and rename.

use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, cli_for_routed_workspace, report_auto_flush_failure,
    resolve_issue_id, retry_mutation_with_jsonl_recovery,
};
use crate::cli::{LabelAddArgs, LabelCommands, LabelListArgs, LabelRemoveArgs, LabelRenameArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig};
use crate::validation::LabelValidator;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use tracing::{debug, info};

/// Execute the label command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(
    command: &LabelCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    match command {
        LabelCommands::Add(args) => execute_routed_label_add(args, cli, ctx, &beads_dir),
        LabelCommands::Remove(args) => execute_routed_label_remove(args, cli, ctx, &beads_dir),
        LabelCommands::List(args) => execute_label_list_command(args, json, cli, ctx, &beads_dir),
        LabelCommands::ListAll => {
            let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            label_list_all(&storage_ctx.storage, json, ctx)
        }
        LabelCommands::Rename(args) => {
            let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            let config_layer = storage_ctx.load_config(cli)?;
            let actor = config::resolve_actor(&config_layer);
            label_rename(args, &mut storage_ctx, &actor, json, ctx)
        }
    }
}

/// Execute read-only label subcommands when the caller already has open storage.
///
/// Returns `Ok(false)` for commands that need the normal routing path.
///
/// # Errors
///
/// Returns an error if the label query fails.
pub fn execute_with_storage(
    command: &LabelCommands,
    json: bool,
    ctx: &OutputContext,
    storage: &SqliteStorage,
) -> Result<bool> {
    match command {
        LabelCommands::ListAll => {
            label_list_all(storage, json, ctx)?;
            Ok(true)
        }
        LabelCommands::List(args) if args.issue.is_none() => {
            label_list_unique(storage, ctx)?;
            Ok(true)
        }
        LabelCommands::Add(_)
        | LabelCommands::Remove(_)
        | LabelCommands::List(_)
        | LabelCommands::Rename(_) => Ok(false),
    }
}

/// JSON output for label add/remove operations.
#[derive(Serialize)]
struct LabelActionResult {
    status: String,
    issue_id: String,
    label: String,
}

struct PreparedLabelRoute {
    issue_inputs: Vec<String>,
    resolved_ids: Vec<String>,
    storage_ctx: config::OpenStorageResult,
    actor: String,
    auto_flush_external: bool,
    _routed_write_lock: RoutedWorkspaceWriteLock,
}

/// JSON output for list-all.
#[derive(Serialize)]
struct LabelCount {
    label: String,
    count: usize,
}

/// JSON output for rename.
#[derive(Serialize)]
struct RenameResult {
    old_name: String,
    new_name: String,
    affected_issues: usize,
}

/// Validate a label name.
///
/// Labels may contain ASCII alphanumeric characters, hyphens, underscores, and colons.
fn validate_label(label: &str) -> Result<()> {
    LabelValidator::validate(label).map_err(|error| BeadsError::validation("label", error.message))
}

fn validate_rename_labels(args: &LabelRenameArgs) -> Result<()> {
    validate_label(&args.old_name)?;
    validate_label(&args.new_name)
}

fn label_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn format_label_action_plain_line(result: &LabelActionResult, action: &str) -> Option<String> {
    let label = label_display_text(&result.label);
    let issue_id = label_display_text(&result.issue_id);
    match (action, result.status.as_str()) {
        ("add", "added") => Some(format!("\u{2713} Added label {label} to {issue_id}")),
        ("add", _) => Some(format!(
            "\u{2713} Label {label} already exists on {issue_id}"
        )),
        ("remove", "removed") => Some(format!("\u{2713} Removed label {label} from {issue_id}")),
        ("remove", _) => Some(format!(
            "\u{2713} Label {label} not found on {issue_id} (no-op)"
        )),
        _ => None,
    }
}

fn format_rename_noop_plain_line(label: &str) -> String {
    format!(
        "Label '{}' already has that name; no changes made.",
        label_display_text(label)
    )
}

fn format_rename_not_found_plain_line(label: &str) -> String {
    format!(
        "Label '{}' not found on any issues.",
        label_display_text(label)
    )
}

fn format_rename_result_plain_line(old_name: &str, new_name: &str, count: usize) -> String {
    format!(
        "\u{2713} Renamed label '{}' to '{}' on {} issue{}",
        label_display_text(old_name),
        label_display_text(new_name),
        count,
        if count == 1 { "" } else { "s" }
    )
}

/// Parse issue IDs and labels from the positional arguments and `-l` flags.
///
/// With `-l` (repeatable or comma-separated) every positional argument is an
/// issue ID. Without it the positional list is `<issue>... <label>...`: the
/// trailing run of arguments that do not have the `<prefix>-<hash>` shape of
/// an issue ID are labels (so `br label add br-abc backend urgent` adds two
/// labels), and at least the last argument is always a label, which keeps the
/// classic `label add <issue> <label>` form working for labels that happen to
/// look like IDs. Labels that contain a dash and might be mistaken for an ID
/// are safest passed with `-l`.
fn parse_issues_and_labels(
    issues: &[String],
    label_flags: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    const USAGE: &str =
        "usage: label add <issue...> <label...> or label add <issue...> -l <label>[,<label>]";

    if !label_flags.is_empty() {
        if issues.is_empty() {
            return Err(BeadsError::validation(
                "issues",
                "at least one issue ID required",
            ));
        }
        let labels: Vec<String> = label_flags
            .iter()
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty())
            .collect();
        if labels.is_empty() {
            return Err(BeadsError::validation(
                "label",
                "at least one label required",
            ));
        }
        return Ok((issues.to_vec(), labels));
    }

    if issues.len() < 2 {
        return Err(BeadsError::validation("arguments", USAGE));
    }

    // Walk back from the end while the argument cannot be an issue ID; keep
    // at least one issue at the front and treat at least the last argument as
    // a label.
    let mut split = issues.len() - 1;
    while split > 1 && crate::util::id::parse_id(&issues[split - 1]).is_err() {
        split -= 1;
    }
    let (issue_ids, labels) = issues.split_at(split);
    Ok((issue_ids.to_vec(), labels.to_vec()))
}

fn execute_routed_label_add(
    args: &LabelAddArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (issue_inputs, labels) = parse_issues_and_labels(&args.issues, &args.label)?;
    for label in &labels {
        validate_label(label)?;
    }
    let mut prepared_routes = prepare_label_routes(&issue_inputs, cli, beads_dir)?;

    // One pass per label keeps the per-issue result ordering contract of the
    // routing reorder (one result per requested input per pass).
    let mut results = Vec::new();
    for label in &labels {
        let mut routed_results = Vec::new();
        for prepared_route in &mut prepared_routes {
            let batch_inputs = prepared_route.issue_inputs.clone();
            let batch_results = label_add(prepared_route, label, ctx)?;
            routed_results.push((batch_inputs, batch_results));
        }
        results.extend(reorder_routed_items_by_requested_inputs(
            &issue_inputs,
            routed_results,
            "label add routing",
        )?);
    }
    if let Some(last_result) = results.last() {
        crate::util::set_last_touched_id(beads_dir, &last_result.issue_id);
    }
    render_label_action_results(&results, "add", ctx);
    Ok(())
}

fn label_add(
    prepared_route: &mut PreparedLabelRoute,
    label: &str,
    ctx: &OutputContext,
) -> Result<Vec<LabelActionResult>> {
    let mut results = Vec::new();
    let mut route_has_mutated = false;

    for issue_id in &prepared_route.resolved_ids {
        info!(issue_id = %issue_id, label = %label, "Adding label");

        let added = retry_mutation_with_jsonl_recovery(
            &mut prepared_route.storage_ctx,
            !route_has_mutated,
            "label add",
            Some(issue_id.as_str()),
            |storage| storage.add_label(issue_id, label, &prepared_route.actor),
        )?;

        debug!(already_exists = !added, "Label status check");

        if added {
            info!(issue_id = %issue_id, label = %label, "Label added");
            route_has_mutated = true;
        }

        results.push(LabelActionResult {
            status: if added { "added" } else { "exists" }.to_string(),
            issue_id: issue_id.clone(),
            label: label.to_string(),
        });
    }

    prepared_route.storage_ctx.flush_no_db_if_dirty()?;
    if prepared_route.auto_flush_external
        && let Err(error) = prepared_route.storage_ctx.auto_flush_if_enabled()
    {
        report_auto_flush_failure(
            ctx,
            &prepared_route.storage_ctx.paths.beads_dir,
            &prepared_route.storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(results)
}

fn execute_routed_label_remove(
    args: &LabelRemoveArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (issue_inputs, labels) = parse_issues_and_labels(&args.issues, &args.label)?;
    for label in &labels {
        validate_label(label)?;
    }
    let mut prepared_routes = prepare_label_routes(&issue_inputs, cli, beads_dir)?;

    let mut results = Vec::new();
    for label in &labels {
        let mut routed_results = Vec::new();
        for prepared_route in &mut prepared_routes {
            let batch_inputs = prepared_route.issue_inputs.clone();
            let batch_results = label_remove(prepared_route, label, ctx)?;
            routed_results.push((batch_inputs, batch_results));
        }
        results.extend(reorder_routed_items_by_requested_inputs(
            &issue_inputs,
            routed_results,
            "label remove routing",
        )?);
    }
    if let Some(last_result) = results.last() {
        crate::util::set_last_touched_id(beads_dir, &last_result.issue_id);
    }
    render_label_action_results(&results, "remove", ctx);
    Ok(())
}

fn label_remove(
    prepared_route: &mut PreparedLabelRoute,
    label: &str,
    ctx: &OutputContext,
) -> Result<Vec<LabelActionResult>> {
    let mut results = Vec::new();
    let mut route_has_mutated = false;

    for issue_id in &prepared_route.resolved_ids {
        info!(issue_id = %issue_id, label = %label, "Removing label");

        let removed = retry_mutation_with_jsonl_recovery(
            &mut prepared_route.storage_ctx,
            !route_has_mutated,
            "label remove",
            Some(issue_id.as_str()),
            |storage| storage.remove_label(issue_id, label, &prepared_route.actor),
        )?;
        if removed {
            route_has_mutated = true;
        }

        results.push(LabelActionResult {
            status: if removed { "removed" } else { "not_found" }.to_string(),
            issue_id: issue_id.clone(),
            label: label.to_string(),
        });
    }

    prepared_route.storage_ctx.flush_no_db_if_dirty()?;
    if prepared_route.auto_flush_external
        && let Err(error) = prepared_route.storage_ctx.auto_flush_if_enabled()
    {
        report_auto_flush_failure(
            ctx,
            &prepared_route.storage_ctx.paths.beads_dir,
            &prepared_route.storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(results)
}

fn execute_label_list_command(
    args: &LabelListArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    if let Some(input) = &args.issue {
        let route = config::routing::resolve_route(input, beads_dir)?;
        let mut route_cli = routed_cli_for_batch(cli, route.is_external);
        let routed_write_lock = acquire_routed_workspace_write_lock(
            &route.beads_dir,
            route.is_external,
            route_cli.lock_timeout,
        )?;
        routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
        let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
        auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli)?;
        let config_layer = storage_ctx.load_config(&route_cli)?;
        let id_config = config::id_config_from_layer(&config_layer);
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
        label_list(args, &storage_ctx.storage, &resolver, json, ctx)
    } else {
        let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
        let config_layer = storage_ctx.load_config(cli)?;
        let id_config = config::id_config_from_layer(&config_layer);
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
        label_list(args, &storage_ctx.storage, &resolver, json, ctx)
    }
}

fn prepare_label_routes(
    issue_inputs: &[String],
    cli: &config::CliOverrides,
    beads_dir: &Path,
) -> Result<Vec<PreparedLabelRoute>> {
    let routed_batches = config::routing::group_issue_inputs_by_route(issue_inputs, beads_dir)?;
    let mut prepared_routes = Vec::new();

    for batch in routed_batches {
        let mut batch_cli = routed_cli_for_batch(cli, batch.is_external);
        let routed_write_lock = acquire_routed_workspace_write_lock(
            &batch.beads_dir,
            batch.is_external,
            batch_cli.lock_timeout,
        )?;
        routed_write_lock.mark_cli_write_lock_held(&mut batch_cli);
        let mut storage_ctx = config::open_storage_with_cli(&batch.beads_dir, &batch_cli)?;
        auto_import_storage_ctx_if_stale(&mut storage_ctx, &batch_cli)?;
        let config_layer = storage_ctx.load_config(&batch_cli)?;
        let id_config = config::id_config_from_layer(&config_layer);
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
        let resolved_ids = batch
            .issue_inputs
            .iter()
            .map(|input| resolve_issue_id(&storage_ctx.storage, &resolver, input))
            .collect::<Result<Vec<_>>>()?;

        prepared_routes.push(PreparedLabelRoute {
            issue_inputs: batch.issue_inputs,
            resolved_ids,
            storage_ctx,
            actor: config::resolve_actor(&config_layer),
            auto_flush_external: batch.is_external,
            _routed_write_lock: routed_write_lock,
        });
    }

    Ok(prepared_routes)
}

fn routed_cli_for_batch(cli: &config::CliOverrides, is_external: bool) -> config::CliOverrides {
    cli_for_routed_workspace(cli, is_external)
}

fn render_label_action_results(
    results: &Vec<LabelActionResult>,
    action: &str,
    ctx: &OutputContext,
) {
    if ctx.is_json() {
        ctx.json_pretty(results);
    } else if ctx.is_toon() {
        ctx.toon(results);
    } else if ctx.is_quiet() {
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_label_action_results_rich(results, action, ctx);
    } else {
        for result in results {
            if let Some(line) = format_label_action_plain_line(result, action) {
                println!("{line}");
            }
        }
    }
}

fn label_list(
    args: &LabelListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if let Some(input) = &args.issue {
        // List labels for a specific issue
        let issue_id = resolve_issue_id(storage, resolver, input)?;
        let labels = storage.get_labels(&issue_id)?;

        if ctx.is_json() {
            ctx.json_pretty(&labels);
        } else if ctx.is_toon() {
            ctx.toon(&labels);
        } else if ctx.is_quiet() {
            return Ok(());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_labels_for_issue_rich(&issue_id, &labels, ctx);
        } else if labels.is_empty() {
            println!("No labels for {}.", label_display_text(&issue_id));
        } else {
            println!("Labels for {}:", label_display_text(&issue_id));
            for label in &labels {
                println!("  {}", label_display_text(label));
            }
        }
    } else {
        label_list_unique(storage, ctx)?;
    }

    Ok(())
}

fn label_list_unique(storage: &SqliteStorage, ctx: &OutputContext) -> Result<()> {
    // List all unique labels (without counts - use list-all for counts)
    let labels_with_counts = storage.get_unique_labels_with_counts()?;
    let unique_labels: Vec<String> = labels_with_counts.into_iter().map(|(l, _)| l).collect();

    if ctx.is_json() {
        ctx.json_pretty(&unique_labels);
    } else if ctx.is_toon() {
        ctx.toon(&unique_labels);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_unique_labels_rich(&unique_labels, ctx);
    } else if unique_labels.is_empty() {
        println!("No labels in project.");
    } else {
        println!("Labels ({} total):", unique_labels.len());
        for label in &unique_labels {
            println!("  {}", label_display_text(label));
        }
    }

    Ok(())
}

fn label_list_all(storage: &SqliteStorage, _json: bool, ctx: &OutputContext) -> Result<()> {
    let labels_with_counts = storage.get_unique_labels_with_counts()?;

    let label_counts: Vec<LabelCount> = labels_with_counts
        .into_iter()
        .map(|(label, count)| LabelCount {
            label,
            count: usize::try_from(count).unwrap_or(0),
        })
        .collect();

    if ctx.is_json() {
        ctx.json_pretty(&label_counts);
    } else if ctx.is_toon() {
        ctx.toon(&label_counts);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_label_counts_rich(&label_counts, ctx);
    } else if label_counts.is_empty() {
        println!("No labels in project.");
    } else {
        println!("Labels ({} total):", label_counts.len());
        for lc in &label_counts {
            println!(
                "  {} ({} issue{})",
                label_display_text(&lc.label),
                lc.count,
                if lc.count == 1 { "" } else { "s" }
            );
        }
    }

    Ok(())
}

fn label_rename(
    args: &LabelRenameArgs,
    storage_ctx: &mut config::OpenStorageResult,
    actor: &str,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let storage = &mut storage_ctx.storage;
    validate_rename_labels(args)?;

    if args.old_name == args.new_name {
        if ctx.is_json() {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            ctx.json_pretty(&result);
        } else if ctx.is_toon() {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            ctx.toon(&result);
        } else if ctx.is_quiet() {
            return Ok(());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_rename_noop_rich(&args.old_name, ctx);
        } else {
            let line = format_rename_noop_plain_line(&args.old_name);
            println!("{line}");
        }
        return Ok(());
    }

    info!(
        old = %args.old_name,
        new = %args.new_name,
        "Renaming label"
    );

    let count = storage.rename_label(&args.old_name, &args.new_name, actor)?;

    if count == 0 {
        if ctx.is_json() {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            ctx.json_pretty(&result);
        } else if ctx.is_toon() {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            ctx.toon(&result);
        } else if ctx.is_quiet() {
            return Ok(());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_rename_not_found_rich(&args.old_name, ctx);
        } else {
            let line = format_rename_not_found_plain_line(&args.old_name);
            println!("{line}");
        }
        return Ok(());
    }

    storage_ctx.flush_no_db_if_dirty()?;

    if ctx.is_json() {
        let result = RenameResult {
            old_name: args.old_name.clone(),
            new_name: args.new_name.clone(),
            affected_issues: count,
        };
        ctx.json_pretty(&result);
    } else if ctx.is_toon() {
        let result = RenameResult {
            old_name: args.old_name.clone(),
            new_name: args.new_name.clone(),
            affected_issues: count,
        };
        ctx.toon(&result);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_rename_result_rich(&args.old_name, &args.new_name, count, ctx);
    } else {
        let line = format_rename_result_plain_line(&args.old_name, &args.new_name, count);
        println!("{line}");
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
                return Err(BeadsError::internal(format!(
                    "{} returned unexpected issue input {}",
                    label_display_text(context),
                    label_display_text(&input)
                )));
            };
            let Some(slot) = ordered_items.get_mut(index) else {
                return Err(BeadsError::internal(format!(
                    "{} returned out-of-range issue input position {}",
                    label_display_text(context),
                    index
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
                let requested_input = requested_inputs
                    .get(index)
                    .map_or("<unknown>", String::as_str);
                BeadsError::internal(format!(
                    "{} did not produce a result for {}",
                    label_display_text(context),
                    label_display_text(requested_input)
                ))
            })
        })
        .collect()
}

// ============================================================================
// Rich Output Rendering Functions
// ============================================================================

/// Get a consistent color for a label based on its name hash.
fn label_color(label: &str) -> Color {
    // Color palette for labels - varied but readable colors
    const LABEL_PALETTE: &[&str] = &[
        "cyan",
        "green",
        "yellow",
        "magenta",
        "blue",
        "bright_cyan",
        "bright_green",
        "bright_yellow",
        "bright_magenta",
        "bright_blue",
    ];

    let hash = label.bytes().fold(0u8, u8::wrapping_add);
    let color_name = LABEL_PALETTE
        .get(usize::from(hash) % LABEL_PALETTE.len())
        .copied()
        .unwrap_or("cyan");
    Color::parse(color_name).unwrap_or_default()
}

/// Render label add/remove action results in rich mode.
fn render_label_action_results_rich(
    results: &[LabelActionResult],
    action: &str,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();

    for result in results {
        let mut text = Text::new("");

        let (icon, verb, style) = match (action, result.status.as_str()) {
            ("add", "added") => ("\u{2713}", "Added", theme.success.clone()),
            ("add", _) => ("\u{2022}", "Exists", theme.dimmed.clone()),
            (_, "removed") => ("\u{2713}", "Removed", theme.success.clone()),
            _ => ("\u{2022}", "Not found", theme.dimmed.clone()),
        };

        text.append_styled(&format!("{icon} {verb} label "), style);
        text.append_styled(
            sanitize_terminal_inline(&result.label).as_ref(),
            Style::new().color(label_color(&result.label)),
        );
        text.append(if action == "add" { " on " } else { " from " });
        text.append_styled(
            &label_display_text(&result.issue_id),
            theme.issue_id.clone(),
        );

        console.print_renderable(&text);
    }
}

/// Render labels for a specific issue in rich mode.
fn render_labels_for_issue_rich(issue_id: &str, labels: &[String], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if labels.is_empty() {
        let mut text = Text::new("");
        text.append_styled("No labels for ", theme.dimmed.clone());
        text.append_styled(&label_display_text(issue_id), theme.issue_id.clone());
        console.print_renderable(&text);
        return;
    }

    let mut text = Text::new("");
    text.append("Labels for ");
    text.append_styled(&label_display_text(issue_id), theme.issue_id.clone());
    text.append(":");
    console.print_renderable(&text);

    // Display labels on a single line with spacing
    let mut label_line = Text::new("  ");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            label_line.append("  ");
        }
        label_line.append_styled(
            &label_display_text(label),
            Style::new().color(label_color(label)),
        );
    }
    console.print_renderable(&label_line);
}

/// Render unique labels list in rich mode.
fn render_unique_labels_rich(labels: &[String], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if labels.is_empty() {
        let text = Text::styled("No labels in project.", theme.dimmed.clone());
        console.print_renderable(&text);
        return;
    }

    let mut header = Text::new("");
    header.append_styled("Labels ", Style::new().bold());
    header.append_styled(&format!("({} total)", labels.len()), theme.dimmed.clone());
    console.print_renderable(&header);

    // Display labels in a compact format
    let mut label_line = Text::new("  ");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            label_line.append("  ");
        }
        label_line.append_styled(
            &label_display_text(label),
            Style::new().color(label_color(label)),
        );
    }
    console.print_renderable(&label_line);
}

/// Render label counts (list-all) in rich mode with Panel.
fn render_label_counts_rich(label_counts: &[LabelCount], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if label_counts.is_empty() {
        let text = Text::styled("No labels in project.", theme.dimmed.clone());
        console.print_renderable(&text);
        return;
    }

    let mut content = Text::new("");

    // Calculate total issues with labels
    let total_issues: usize = label_counts.iter().map(|lc| lc.count).sum();

    for (i, lc) in label_counts.iter().enumerate() {
        if i > 0 {
            content.append("\n");
        }
        content.append_styled(
            &format!("{:<20}", label_display_text(&lc.label)),
            Style::new().color(label_color(&lc.label)),
        );
        content.append_styled(
            &format!(
                "{:>4} issue{}",
                lc.count,
                if lc.count == 1 { "" } else { "s" }
            ),
            theme.dimmed.clone(),
        );
    }

    content.append("\n\n");
    content.append_styled(
        &format!(
            "Total: {} label{} across {} issue assignment{}",
            label_counts.len(),
            if label_counts.len() == 1 { "" } else { "s" },
            total_issues,
            if total_issues == 1 { "" } else { "s" }
        ),
        theme.dimmed.clone(),
    );

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::new("Project Labels"))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render rename not found message in rich mode.
fn render_rename_not_found_rich(old_name: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{26a0} ", theme.warning.clone());
    text.append("Label ");
    text.append_styled(
        sanitize_terminal_inline(old_name).as_ref(),
        Style::new().color(label_color(old_name)),
    );
    text.append_styled(" not found on any issues.", theme.dimmed.clone());

    console.print_renderable(&text);
}

/// Render rename no-op message in rich mode.
fn render_rename_noop_rich(label: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2139} ", theme.dimmed.clone());
    text.append("Label ");
    text.append_styled(
        sanitize_terminal_inline(label).as_ref(),
        Style::new().color(label_color(label)),
    );
    text.append_styled(
        " already has that name; no changes made.",
        theme.dimmed.clone(),
    );

    console.print_renderable(&text);
}

/// Render rename result in rich mode.
fn render_rename_result_rich(old_name: &str, new_name: &str, count: usize, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("Renamed ");
    text.append_styled(
        sanitize_terminal_inline(old_name).as_ref(),
        Style::new().color(label_color(old_name)).dim(),
    );
    text.append(" \u{2192} ");
    text.append_styled(
        sanitize_terminal_inline(new_name).as_ref(),
        Style::new().color(label_color(new_name)).bold(),
    );
    text.append_styled(
        &format!(" on {} issue{}", count, if count == 1 { "" } else { "s" }),
        theme.dimmed.clone(),
    );

    console.print_renderable(&text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_display_text_sanitizes_terminal_controls() {
        let rendered = label_display_text("urgent\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn label_plain_output_helpers_escape_terminal_controls() {
        let result = LabelActionResult {
            status: "added".to_string(),
            issue_id: "bd-1\x1b[2J".to_string(),
            label: "urgent\x07".to_string(),
        };

        let lines = [
            format_label_action_plain_line(&result, "add").expect("add line"),
            format_rename_noop_plain_line("old\x1b[2J"),
            format_rename_not_found_plain_line("missing\rlabel"),
            format_rename_result_plain_line("old\x08", "new\nlabel", 2),
        ];

        for line in lines {
            assert!(!line.chars().any(char::is_control));
        }
    }

    #[test]
    fn test_validate_label_valid() {
        assert!(validate_label("bug").is_ok());
        assert!(validate_label("high-priority").is_ok());
        assert!(validate_label("needs_review").is_ok());
        assert!(validate_label("v1_0").is_ok());
        assert!(validate_label("Bug123").is_ok());
        assert!(validate_label("team:backend").is_ok());
    }

    #[test]
    fn test_validate_label_invalid() {
        assert!(validate_label("").is_err());
        assert!(validate_label("has space").is_err());
        assert!(validate_label("has/slash").is_err());
        assert!(validate_label("special@char").is_err());
        assert!(validate_label("dot.not.allowed").is_err());
        assert!(validate_label(&"a".repeat(51)).is_err());
    }

    #[test]
    fn test_validate_rename_labels_rejects_invalid_old_name() {
        let args = LabelRenameArgs {
            old_name: "bad label".to_string(),
            new_name: "backend".to_string(),
        };
        let err = validate_rename_labels(&args).unwrap_err();

        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn test_validate_rename_labels_rejects_invalid_new_name() {
        let args = LabelRenameArgs {
            old_name: "backend".to_string(),
            new_name: "bad label".to_string(),
        };
        let err = validate_rename_labels(&args).unwrap_err();

        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn test_label_remove_rejects_invalid_label_before_route_preparation() {
        let args = LabelRemoveArgs {
            issues: vec!["bd-abc".to_string(), "has space".to_string()],
            label: Vec::new(),
        };
        let ctx = OutputContext::from_flags(false, false, true);
        let err = execute_routed_label_remove(
            &args,
            &config::CliOverrides::default(),
            &ctx,
            std::path::Path::new("/missing/.beads"),
        )
        .unwrap_err();

        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn test_validate_label_namespaced_allows_provides() {
        assert!(validate_label("provides:auth").is_ok());
        assert!(validate_label("provides:").is_ok());
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn test_parse_issues_and_labels_with_flag() {
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "bd-def"]), &strings(&["urgent"]))
                .unwrap();
        assert_eq!(issues, vec!["bd-abc", "bd-def"]);
        assert_eq!(labels, vec!["urgent"]);
    }

    #[test]
    fn test_parse_issues_and_labels_with_repeated_and_delimited_flags() {
        // clap splits `-l a,b` into two values; `-l a -l b` arrives the same way.
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc"]), &strings(&["backend", "urgent"]))
                .unwrap();
        assert_eq!(issues, vec!["bd-abc"]);
        assert_eq!(labels, vec!["backend", "urgent"]);
    }

    #[test]
    fn test_parse_issues_and_labels_positional_single_label() {
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "bd-def", "urgent"]), &[]).unwrap();
        assert_eq!(issues, vec!["bd-abc", "bd-def"]);
        assert_eq!(labels, vec!["urgent"]);
    }

    #[test]
    fn test_parse_issues_and_labels_positional_multiple_labels() {
        // README form: `br label add br-abc123 backend urgent`.
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "backend", "urgent"]), &[]).unwrap();
        assert_eq!(issues, vec!["bd-abc"]);
        assert_eq!(labels, vec!["backend", "urgent"]);
    }

    #[test]
    fn test_parse_issues_and_labels_positional_keeps_leading_issue() {
        // Every positional looks like a label except the first: the first is
        // still the issue and the rest are labels.
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "one", "two", "three"]), &[]).unwrap();
        assert_eq!(issues, vec!["bd-abc"]);
        assert_eq!(labels, vec!["one", "two", "three"]);
    }

    #[test]
    fn test_parse_issues_and_labels_id_like_label_uses_last_argument() {
        // A label that parses as an issue ID falls back to the classic rule:
        // the last argument is the label.
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "needs-review"]), &[]).unwrap();
        assert_eq!(issues, vec!["bd-abc"]);
        assert_eq!(labels, vec!["needs-review"]);
    }

    #[test]
    fn test_parse_issues_and_labels_single_issue() {
        let (issues, labels) =
            parse_issues_and_labels(&strings(&["bd-abc", "urgent"]), &[]).unwrap();
        assert_eq!(issues, vec!["bd-abc"]);
        assert_eq!(labels, vec!["urgent"]);
    }

    #[test]
    fn test_parse_issues_and_labels_missing_label() {
        assert!(parse_issues_and_labels(&strings(&["bd-abc"]), &[]).is_err());
    }

    #[test]
    fn test_parse_issues_and_labels_no_issues_with_flag() {
        assert!(parse_issues_and_labels(&[], &strings(&["urgent"])).is_err());
    }

    #[test]
    fn test_parse_issues_and_labels_blank_flag_values_rejected() {
        assert!(parse_issues_and_labels(&strings(&["bd-abc"]), &strings(&[" ", ""])).is_err());
    }
}
