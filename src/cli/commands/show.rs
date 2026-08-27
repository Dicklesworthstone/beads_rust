//! Show command implementation.

use crate::cli::commands::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    cli_for_routed_workspace, external_project_db_paths_after_auto_import_if_needed,
};
use crate::cli::{ShowArgs, resolve_output_format_basic_with_outer_mode};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{
    IssueDetails, IssueWithDependencyMetadata, format_priority_label, format_status_icon_colored,
    format_status_label, format_type_label, sanitize_terminal_inline, sanitize_terminal_text,
};
use crate::model::{Dependency, Issue, Priority, Status};
use crate::output::{IssuePanel, OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::sync::{get_issue_ids_from_jsonl, path as sync_path, read_issues_from_jsonl};
use crate::util::id::{IdResolver, ResolverConfig, normalize_id};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Execute the show command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or issues are not found.
pub fn execute(
    args: &ShowArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    execute_routed(args, cli, outer_ctx, &beads_dir, None, None)
}

/// Execute show using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if issue resolution or rendering fails.
pub fn execute_with_storage(
    args: &ShowArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage: &SqliteStorage,
) -> Result<()> {
    execute_routed(args, cli, outer_ctx, beads_dir, Some(storage), None)
}

/// Execute show using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error if issue resolution or rendering fails.
pub fn execute_with_storage_ctx(
    args: &ShowArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_routed(args, cli, outer_ctx, beads_dir, None, Some(storage_ctx))
}

#[allow(clippy::too_many_lines)]
fn execute_routed(
    args: &ShowArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    let target_ids = requested_target_ids(args, beads_dir)?;
    let routed_batches = config::routing::group_issue_inputs_by_route(&target_ids, beads_dir)?;
    if !routed_batches.iter().any(|batch| batch.is_external) {
        return execute_inner(
            args,
            cli,
            outer_ctx,
            beads_dir,
            preloaded_storage,
            preloaded_storage_ctx,
        );
    }

    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let quiet = cli.quiet.unwrap_or(false);
    let normalized_local_beads_dir =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());

    if matches!(
        output_format,
        crate::cli::OutputFormat::Json | crate::cli::OutputFormat::Toon
    ) {
        let mut routed_details = Vec::new();
        for batch in routed_batches {
            let mut batch_args = args.clone();
            batch_args.ids.clone_from(&batch.issue_inputs);

            let batch_beads_dir = batch.beads_dir;
            let normalized_batch_beads_dir =
                dunce::canonicalize(&batch_beads_dir).unwrap_or_else(|_| batch_beads_dir.clone());
            let use_preloaded = normalized_batch_beads_dir == normalized_local_beads_dir;
            let mut batch_cli = cli_for_routed_workspace(cli, !use_preloaded);
            let routed_write_lock = acquire_routed_workspace_write_lock(
                &batch_beads_dir,
                !use_preloaded,
                batch_cli.lock_timeout,
            )?;
            routed_write_lock.mark_cli_write_lock_held(&mut batch_cli);
            let (batch_details, _) = load_issue_details_for_route(
                &batch_args,
                &batch_cli,
                &batch_beads_dir,
                if use_preloaded {
                    preloaded_storage
                } else {
                    None
                },
                if use_preloaded {
                    preloaded_storage_ctx
                } else {
                    None
                },
            )?;
            let mut batch_details = batch_details;
            attach_inherited_context(
                &mut batch_details,
                &batch_beads_dir,
                &batch_cli,
                if use_preloaded {
                    preloaded_storage
                } else {
                    None
                },
                if use_preloaded {
                    preloaded_storage_ctx
                } else {
                    None
                },
            );
            routed_details.push((batch.issue_inputs, batch_details));
        }
        let details_list =
            reorder_routed_items_by_requested_inputs(&target_ids, routed_details, "show routing")?;
        let structured_ctx = OutputContext::from_output_format(output_format, quiet, true);

        match output_format {
            crate::cli::OutputFormat::Json => structured_ctx.json_array(details_list.iter()),
            crate::cli::OutputFormat::Toon => {
                structured_ctx.toon_with_stats(&details_list, args.stats);
            }
            other => {
                return Err(crate::error::BeadsError::internal(format!(
                    "routed show: format '{other:?}' should be handled by the text rendering path"
                )));
            }
        }
        return Ok(());
    }

    let mut routed_render_items = Vec::new();
    for batch in routed_batches {
        let mut batch_args = args.clone();
        batch_args.ids.clone_from(&batch.issue_inputs);

        let normalized_batch_beads_dir =
            dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
        let use_preloaded = normalized_batch_beads_dir == normalized_local_beads_dir;
        let mut batch_cli = cli_for_routed_workspace(cli, !use_preloaded);
        let routed_write_lock = acquire_routed_workspace_write_lock(
            &batch.beads_dir,
            !use_preloaded,
            batch_cli.lock_timeout,
        )?;
        routed_write_lock.mark_cli_write_lock_held(&mut batch_cli);
        let (batch_details, use_color) = load_issue_details_for_route(
            &batch_args,
            &batch_cli,
            &batch.beads_dir,
            if use_preloaded {
                preloaded_storage
            } else {
                None
            },
            if use_preloaded {
                preloaded_storage_ctx
            } else {
                None
            },
        )?;
        routed_render_items.push((
            batch.issue_inputs,
            batch_details
                .into_iter()
                .map(|details| (details, use_color))
                .collect(),
        ));
    }

    let ordered_details =
        reorder_routed_items_by_requested_inputs(&target_ids, routed_render_items, "show routing")?;

    if quiet {
        return Ok(());
    }

    for (index, (details, use_color)) in ordered_details.iter().enumerate() {
        if index > 0 {
            println!();
        }

        let ctx = OutputContext::from_output_format(output_format, quiet, !*use_color);
        if matches!(ctx.mode(), OutputMode::Rich) {
            let panel = IssuePanel::from_details(details, ctx.theme());
            panel.print(&ctx, !args.no_wrap);
        } else {
            print_issue_details(details, *use_color, !args.no_wrap);
        }
    }

    Ok(())
}

fn requested_target_ids(args: &ShowArgs, beads_dir: &Path) -> Result<Vec<String>> {
    let mut target_ids = args.ids.clone();
    if target_ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_ids.push(last_touched);
    }
    Ok(target_ids)
}

/// Populate each issue's `inherited_context` field with the ancestor
/// `agent_context` blocks (beads_rust#297) so JSON/TOON output carries the
/// same governing context that text mode renders (beads_rust#430).
///
/// No-op unless the project has opted in to inherited-context emission.
/// Prefers the caller's preloaded storage; falls back to a transient
/// read connection, and silently skips on open failure — the inherited
/// blocks are an optional enrichment, not worth failing the whole show.
/// Unlike the text renderer, blocks are NOT deduplicated across issues:
/// each structured record must be complete on its own for programmatic
/// consumers.
fn attach_inherited_context(
    details_list: &mut [IssueDetails],
    beads_dir: &Path,
    cli: &config::CliOverrides,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) {
    if details_list.is_empty() || !crate::inheritance::is_enabled(beads_dir) {
        return;
    }
    let transient_ctx = if preloaded_storage.is_none() && preloaded_storage_ctx.is_none() {
        config::open_storage_with_cli(beads_dir, cli).ok()
    } else {
        None
    };
    let Some(storage) = preloaded_storage
        .or_else(|| preloaded_storage_ctx.map(|ctx| &ctx.storage))
        .or_else(|| transient_ctx.as_ref().map(|ctx| &ctx.storage))
    else {
        return;
    };
    for details in details_list.iter_mut() {
        if let Ok(blocks) = crate::inheritance::collect_inherited_blocks(storage, &details.issue.id)
        {
            details.inherited_context = blocks;
        }
    }
}

fn execute_inner(
    args: &ShowArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    let (mut details_list, use_color) = load_issue_details_for_route(
        args,
        cli,
        beads_dir,
        preloaded_storage,
        preloaded_storage_ctx,
    )?;
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }
    // beads_rust#430: structured output must carry the same inherited
    // governing context that text mode renders, as a structured field.
    if matches!(
        output_format,
        crate::cli::OutputFormat::Json | crate::cli::OutputFormat::Toon
    ) {
        attach_inherited_context(
            &mut details_list,
            beads_dir,
            cli,
            preloaded_storage,
            preloaded_storage_ctx,
        );
    }
    match output_format {
        crate::cli::OutputFormat::Json => {
            ctx.json_array(details_list.iter());
        }
        crate::cli::OutputFormat::Toon => {
            ctx.toon_with_stats(&details_list, args.stats);
        }
        crate::cli::OutputFormat::Text | crate::cli::OutputFormat::Csv => {
            // beads_rust#297: emit inherited governing context for each
            // bead before its own details, when the project has opted
            // in. Prefer the caller's preloaded storage; fall back to
            // opening a transient read connection so the feature works
            // from both `br show` and `br show --workspace …` paths.
            // Failure to open is non-fatal — the alternative would be
            // failing the entire show over an optional feature.
            let inheritance_enabled = crate::inheritance::is_enabled(beads_dir);
            let transient_ctx = if inheritance_enabled
                && preloaded_storage.is_none()
                && preloaded_storage_ctx.is_none()
            {
                config::open_storage_with_cli(beads_dir, cli).ok()
            } else {
                None
            };
            let inheritance_storage: Option<&SqliteStorage> = preloaded_storage
                .or_else(|| preloaded_storage_ctx.map(|ctx| &ctx.storage))
                .or_else(|| transient_ctx.as_ref().map(|ctx| &ctx.storage));
            // beads_rust#351: when showing several siblings, each child's
            // ancestor chain resolves independently, so the same epic/parent
            // block would be re-rendered once per sibling. Dedup across the
            // whole invocation: each inherited source is emitted exactly
            // once, before the first child that references it.
            let mut emitted_sources: HashSet<String> = HashSet::new();
            for (i, details) in details_list.iter().enumerate() {
                if i > 0 {
                    println!(); // Separate multiple issues
                }
                if inheritance_enabled
                    && let Some(storage) = inheritance_storage
                    && let Ok(mut blocks) =
                        crate::inheritance::collect_inherited_blocks(storage, &details.issue.id)
                {
                    blocks.retain(|block| emitted_sources.insert(block.source_id.clone()));
                    if !blocks.is_empty() {
                        let rendered = crate::inheritance::render_text(&blocks);
                        print!("{rendered}");
                        if !rendered.ends_with('\n') {
                            println!();
                        }
                    }
                }
                if matches!(ctx.mode(), OutputMode::Rich) {
                    let panel = IssuePanel::from_details(details, ctx.theme());
                    panel.print(&ctx, !args.no_wrap);
                } else {
                    print_issue_details(details, use_color, !args.no_wrap);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
fn reorder_details_by_requested_inputs(
    requested_inputs: &[String],
    routed_details: Vec<(Vec<String>, Vec<IssueDetails>)>,
) -> Result<Vec<IssueDetails>> {
    reorder_routed_items_by_requested_inputs(requested_inputs, routed_details, "show routing")
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

    let mut ordered_details: Vec<Option<T>> = (0..requested_inputs.len()).map(|_| None).collect();
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
                    "{context} returned unexpected issue input {input}"
                )));
            };
            let slot = ordered_details.get_mut(index).ok_or_else(|| {
                BeadsError::internal(format!(
                    "{context} returned out-of-range issue index {index}"
                ))
            })?;
            *slot = Some(item);
        }
    }

    ordered_details
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.ok_or_else(|| {
                BeadsError::internal(format!(
                    "{context} did not produce a result for {}",
                    requested_inputs[index]
                ))
            })
        })
        .collect()
}

fn load_issue_details_for_route(
    args: &ShowArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<(Vec<IssueDetails>, bool)> {
    let target_ids = requested_target_ids(args, beads_dir)?;

    if let Some(storage_ctx) = preloaded_storage_ctx {
        let config_layer = storage_ctx.load_config(cli)?;
        let use_color = config::should_use_color(&config_layer);
        let id_config = config::id_config_from_layer(&config_layer);
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
        let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
            &storage_ctx.storage,
            &config_layer,
            beads_dir,
            cli,
        )?;
        let details_list = load_issue_details_from_storage(
            &target_ids,
            &resolver,
            &storage_ctx.storage,
            &storage_ctx.paths.jsonl_path,
            &external_db_paths,
        )?;
        return Ok((details_list, use_color));
    }

    let startup = config::load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let mut bootstrap_config = startup.merged_config.clone();
    bootstrap_config.merge_from(&cli.as_layer());
    let no_db = config::no_db_from_layer(&bootstrap_config).unwrap_or(false);
    let mut owned_storage_ctx = if no_db || preloaded_storage.is_some() {
        None
    } else {
        Some(config::open_storage_with_cli(beads_dir, cli)?)
    };
    if let Some(storage_ctx) = owned_storage_ctx.as_mut() {
        auto_import_storage_ctx_if_stale(storage_ctx, cli)?;
    }
    let config_layer = if let Some(storage_ctx) = owned_storage_ctx.as_ref() {
        storage_ctx.load_config(cli)?
    } else {
        config::load_config(beads_dir, preloaded_storage, cli)?
    };
    let use_color = config::should_use_color(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let details_list = if no_db {
        let external_db_paths = config::external_project_db_paths(&config_layer, beads_dir);
        load_issue_details_from_jsonl(
            &target_ids,
            &resolver,
            &startup.paths.jsonl_path,
            &external_db_paths,
        )?
    } else {
        let storage = preloaded_storage.unwrap_or_else(|| {
            &owned_storage_ctx
                .as_ref()
                .expect("show should have an open storage handle")
                .storage
        });
        let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
            storage,
            &config_layer,
            beads_dir,
            cli,
        )?;
        load_issue_details_from_storage(
            &target_ids,
            &resolver,
            storage,
            &startup.paths.jsonl_path,
            &external_db_paths,
        )?
    };

    Ok((details_list, use_color))
}

fn load_issue_details_from_storage(
    target_ids: &[String],
    resolver: &IdResolver,
    storage: &SqliteStorage,
    jsonl_path: &Path,
    external_db_paths: &HashMap<String, PathBuf>,
) -> Result<Vec<IssueDetails>> {
    let mut external_statuses: Option<HashMap<String, bool>> = None;
    let mut details_list = Vec::with_capacity(target_ids.len());

    for id_input in target_ids {
        let resolution = match resolver.resolve_fallible(
            id_input,
            |id| storage.id_exists(id),
            |hash| storage.find_ids_by_hash(hash),
        ) {
            Ok(resolution) => resolution,
            Err(BeadsError::IssueNotFound { id }) => {
                fail_if_jsonl_contains_missing_database_id(
                    jsonl_path,
                    resolver,
                    &[id_input.as_str(), id.as_str()],
                )?;
                return Err(BeadsError::IssueNotFound { id });
            }
            Err(error) => return Err(error),
        };

        let Some(mut details) = storage.get_issue_details(&resolution.id, true, false, 10)? else {
            fail_if_jsonl_contains_missing_database_id(
                jsonl_path,
                resolver,
                &[resolution.id.as_str()],
            )?;
            return Err(BeadsError::IssueNotFound { id: resolution.id });
        };

        if issue_details_have_external_dependencies(&details) {
            if external_statuses.is_none() {
                external_statuses =
                    Some(storage.resolve_external_dependency_statuses(external_db_paths, false)?);
            }
            if let Some(statuses) = external_statuses.as_ref() {
                apply_external_dependency_metadata(&mut details.dependencies, statuses);
                apply_external_dependency_metadata(&mut details.dependents, statuses);
            }
        }

        details_list.push(details);
    }

    Ok(details_list)
}

fn fail_if_jsonl_contains_missing_database_id(
    jsonl_path: &Path,
    resolver: &IdResolver,
    candidate_ids: &[&str],
) -> Result<()> {
    if !jsonl_path.is_file() {
        return Ok(());
    }
    let jsonl_ids = get_issue_ids_from_jsonl(jsonl_path)?
        .into_iter()
        .collect::<Vec<_>>();
    for candidate in candidate_ids {
        match resolver.resolve_fallible(
            candidate,
            |id| Ok(jsonl_ids.iter().any(|known| known == id)),
            |hash| Ok(crate::util::id::find_matching_ids(&jsonl_ids, hash)),
        ) {
            Ok(resolution) => {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Database/JSONL divergence: issue {} exists in JSONL but is not addressable in SQLite; refusing a false missing-issue result. Run `br doctor --repair` against a preserved workspace copy before trusting database-backed reads",
                        resolution.id
                    ),
                });
            }
            Err(BeadsError::AmbiguousId { matches, .. }) if !matches.is_empty() => {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Database/JSONL divergence: {candidate:?} matches {} issue IDs in JSONL but none are addressable in SQLite; refusing a false missing-issue result. Run `br doctor --repair` against a preserved workspace copy before trusting database-backed reads",
                        matches.len()
                    ),
                });
            }
            Err(BeadsError::IssueNotFound { .. } | BeadsError::InvalidId { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn load_issue_details_from_jsonl(
    target_ids: &[String],
    resolver: &IdResolver,
    jsonl_path: &Path,
    external_db_paths: &HashMap<String, PathBuf>,
) -> Result<Vec<IssueDetails>> {
    if let Some(details_list) =
        load_exact_issue_details_from_jsonl(target_ids, resolver, jsonl_path, external_db_paths)?
    {
        return Ok(details_list);
    }

    load_issue_details_from_jsonl_materialized(target_ids, resolver, jsonl_path, external_db_paths)
}

fn load_issue_details_from_jsonl_materialized(
    target_ids: &[String],
    resolver: &IdResolver,
    jsonl_path: &Path,
    external_db_paths: &HashMap<String, PathBuf>,
) -> Result<Vec<IssueDetails>> {
    let issues = read_issues_from_jsonl(jsonl_path)?;
    let mut issues_by_id = HashMap::with_capacity(issues.len());
    for issue in issues {
        issues_by_id.insert(issue.id.clone(), issue);
    }

    let mut details_list = Vec::with_capacity(target_ids.len());
    for id_input in target_ids {
        let resolution = resolver.resolve_fallible(
            id_input,
            |id| Ok(issues_by_id.contains_key(id)),
            |hash| Ok(find_ids_by_hash_in_memory(&issues_by_id, hash)),
        )?;
        let issue = issues_by_id
            .get(&resolution.id)
            .ok_or_else(|| BeadsError::IssueNotFound {
                id: resolution.id.clone(),
            })?;
        details_list.push(build_issue_details_from_jsonl(issue, &issues_by_id)?);
    }

    let external_ids = collect_external_dependency_ids(&details_list);
    if !external_ids.is_empty() {
        let statuses = SqliteStorage::resolve_external_dependency_statuses_for_ids(
            &external_ids,
            external_db_paths,
        );
        for details in &mut details_list {
            apply_external_dependency_metadata(&mut details.dependencies, &statuses);
            apply_external_dependency_metadata(&mut details.dependents, &statuses);
        }
    }

    Ok(details_list)
}

#[derive(Clone)]
struct JsonlIssueSummary {
    id: String,
    title: String,
    status: Status,
    priority: Priority,
    created_at: DateTime<Utc>,
}

impl JsonlIssueSummary {
    fn from_issue(issue: &Issue) -> Self {
        Self {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status.clone(),
            priority: issue.priority,
            created_at: issue.created_at,
        }
    }

    fn dependency_metadata(&self, dep_type: &str) -> IssueWithDependencyMetadata {
        IssueWithDependencyMetadata {
            id: self.id.clone(),
            title: self.title.clone(),
            status: self.status.clone(),
            priority: self.priority,
            dep_type: dep_type.to_string(),
        }
    }
}

type JsonlDependencyDisplayEntry = (IssueWithDependencyMetadata, Priority, DateTime<Utc>);

const MAX_EXACT_JSONL_SHOW_INITIAL_CAPACITY: usize = 64 * 1024;

struct ExactJsonlShowIndex {
    summaries: HashMap<String, JsonlIssueSummary>,
    targets: HashMap<String, Issue>,
    dependents: HashMap<String, Vec<JsonlDependencyDisplayEntry>>,
}

fn load_exact_issue_details_from_jsonl(
    target_ids: &[String],
    resolver: &IdResolver,
    jsonl_path: &Path,
    external_db_paths: &HashMap<String, PathBuf>,
) -> Result<Option<Vec<IssueDetails>>> {
    let Some(direct_target_ids) = direct_jsonl_target_ids(target_ids, resolver) else {
        return Ok(None);
    };
    let direct_target_set = direct_target_ids.iter().cloned().collect::<HashSet<_>>();
    let index = scan_exact_jsonl_show_index(jsonl_path, &direct_target_set)?;

    if direct_target_ids
        .iter()
        .any(|id| !index.targets.contains_key(id))
    {
        return Ok(None);
    }

    let mut details_list = direct_target_ids
        .iter()
        .map(|id| build_issue_details_from_exact_jsonl_index(id, &index))
        .collect::<Result<Vec<_>>>()?;

    let external_ids = collect_external_dependency_ids(&details_list);
    if !external_ids.is_empty() {
        let statuses = SqliteStorage::resolve_external_dependency_statuses_for_ids(
            &external_ids,
            external_db_paths,
        );
        for details in &mut details_list {
            apply_external_dependency_metadata(&mut details.dependencies, &statuses);
            apply_external_dependency_metadata(&mut details.dependents, &statuses);
        }
    }

    Ok(Some(details_list))
}

fn direct_jsonl_target_ids(target_ids: &[String], resolver: &IdResolver) -> Option<Vec<String>> {
    let mut direct_ids = Vec::with_capacity(target_ids.len());
    for id_input in target_ids {
        let trimmed = id_input.trim();
        if trimmed.is_empty() {
            return None;
        }

        let normalized = normalize_id(trimmed);
        if normalized.contains('-') {
            direct_ids.push(normalized);
        } else {
            direct_ids.push(format!("{}-{normalized}", resolver.default_prefix()));
        }
    }
    Some(direct_ids)
}

fn scan_exact_jsonl_show_index(
    jsonl_path: &Path,
    target_ids: &HashSet<String>,
) -> Result<ExactJsonlShowIndex> {
    let file = File::open(jsonl_path)?;
    sync_path::validate_jsonl_fd_metadata(&file, jsonl_path)?;
    let file_size = file.metadata().map_or(0, |metadata| metadata.len());
    let estimated_count = estimated_jsonl_show_capacity(file_size);
    let mut reader = BufReader::new(file);
    let mut summaries_by_id = HashMap::with_capacity(estimated_count);
    let mut target_issues_by_id = HashMap::with_capacity(target_ids.len());
    let mut dependents_by_target_id: HashMap<String, Vec<JsonlDependencyDisplayEntry>> =
        HashMap::new();
    let mut seen_ids = HashSet::with_capacity(estimated_count);
    let mut line = String::new();
    let mut line_num = 0;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            line_num += 1;
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed).map_err(|error| {
            BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num + 1, error))
        })?;
        let issue_id = issue.id.clone();
        if !seen_ids.insert(issue_id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue_id,
                jsonl_path.display(),
                line_num + 1
            )));
        }

        let summary = JsonlIssueSummary::from_issue(&issue);
        for dep in &issue.dependencies {
            if target_ids.contains(&dep.depends_on_id) {
                dependents_by_target_id
                    .entry(dep.depends_on_id.clone())
                    .or_default()
                    .push((
                        summary.dependency_metadata(dep.dep_type.as_str()),
                        summary.priority,
                        summary.created_at,
                    ));
            }
        }

        if target_ids.contains(&issue_id) {
            target_issues_by_id.insert(issue_id.clone(), issue);
        }
        summaries_by_id.insert(issue_id, summary);
        line_num += 1;
    }

    Ok(ExactJsonlShowIndex {
        summaries: summaries_by_id,
        targets: target_issues_by_id,
        dependents: dependents_by_target_id,
    })
}

fn estimated_jsonl_show_capacity(file_size: u64) -> usize {
    usize::try_from(file_size / 500)
        .unwrap_or(usize::MAX)
        .min(MAX_EXACT_JSONL_SHOW_INITIAL_CAPACITY)
}

fn build_issue_details_from_exact_jsonl_index(
    issue_id: &str,
    index: &ExactJsonlShowIndex,
) -> Result<IssueDetails> {
    let issue = index
        .targets
        .get(issue_id)
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: issue_id.to_string(),
        })?;

    let mut dependencies = issue
        .dependencies
        .iter()
        .map(|dep| dependency_metadata_from_jsonl_summary(dep, &index.summaries, true))
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.id.cmp(&right.0.id))
    });

    let mut dependents = index.dependents.get(issue_id).cloned().unwrap_or_default();
    dependents.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.id.cmp(&right.0.id))
    });

    let mut issue_without_relations = issue.clone();
    let labels = issue_without_relations.labels.clone();
    let comments = issue_without_relations.comments.clone();
    issue_without_relations.labels.clear();
    issue_without_relations.dependencies.clear();
    issue_without_relations.comments.clear();

    Ok(IssueDetails {
        issue: issue_without_relations,
        labels,
        dependencies: dependencies.into_iter().map(|(item, _, _)| item).collect(),
        dependents: dependents.into_iter().map(|(item, _, _)| item).collect(),
        comments,
        events: Vec::new(),
        parent: issue
            .dependencies
            .iter()
            .rev()
            .find(|dep| dep.dep_type.as_str() == "parent-child")
            .map(|dep| dep.depends_on_id.clone()),
        // Rollup derives from the local database; JSONL fallback paths
        // deliberately omit it.
        rollup: None,
        inherited_context: Vec::new(),
    })
}

fn build_issue_details_from_jsonl(
    issue: &Issue,
    issues_by_id: &HashMap<String, Issue>,
) -> Result<IssueDetails> {
    let mut dependencies = issue
        .dependencies
        .iter()
        .map(|dep| dependency_metadata_from_jsonl(dep, issues_by_id, true))
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.id.cmp(&right.0.id))
    });

    let mut dependents = issues_by_id
        .values()
        .flat_map(|candidate| {
            candidate
                .dependencies
                .iter()
                .filter(move |dep| dep.depends_on_id == issue.id)
                .map(move |dep| (candidate, dep))
        })
        .map(|(candidate, dep)| {
            Ok((
                IssueWithDependencyMetadata {
                    id: candidate.id.clone(),
                    title: candidate.title.clone(),
                    status: candidate.status.clone(),
                    priority: candidate.priority,
                    dep_type: dep.dep_type.as_str().to_string(),
                },
                candidate.priority,
                candidate.created_at,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    dependents.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.id.cmp(&right.0.id))
    });

    let mut issue_without_relations = issue.clone();
    let labels = issue_without_relations.labels.clone();
    let comments = issue_without_relations.comments.clone();
    issue_without_relations.labels.clear();
    issue_without_relations.dependencies.clear();
    issue_without_relations.comments.clear();

    Ok(IssueDetails {
        issue: issue_without_relations,
        labels,
        dependencies: dependencies.into_iter().map(|(item, _, _)| item).collect(),
        dependents: dependents.into_iter().map(|(item, _, _)| item).collect(),
        comments,
        events: Vec::new(),
        parent: issue
            .dependencies
            .iter()
            .rev()
            .find(|dep| dep.dep_type.as_str() == "parent-child")
            .map(|dep| dep.depends_on_id.clone()),
        // Rollup derives from the local database; JSONL fallback paths
        // deliberately omit it.
        rollup: None,
        inherited_context: Vec::new(),
    })
}

fn dependency_metadata_from_jsonl(
    dep: &Dependency,
    issues_by_id: &HashMap<String, Issue>,
    allow_external_placeholder: bool,
) -> (IssueWithDependencyMetadata, Priority, DateTime<Utc>) {
    if let Some(target) = issues_by_id.get(&dep.depends_on_id) {
        return (
            IssueWithDependencyMetadata {
                id: target.id.clone(),
                title: target.title.clone(),
                status: target.status.clone(),
                priority: target.priority,
                dep_type: dep.dep_type.as_str().to_string(),
            },
            target.priority,
            target.created_at,
        );
    }

    if allow_external_placeholder && dep.depends_on_id.starts_with("external:") {
        return (
            IssueWithDependencyMetadata {
                id: dep.depends_on_id.clone(),
                title: dep
                    .depends_on_id
                    .strip_prefix("external:")
                    .unwrap_or(&dep.depends_on_id)
                    .to_string(),
                status: Status::Blocked,
                priority: Priority::MEDIUM,
                dep_type: dep.dep_type.as_str().to_string(),
            },
            Priority::MEDIUM,
            dep.created_at,
        );
    }

    (
        IssueWithDependencyMetadata {
            id: dep.depends_on_id.clone(),
            title: format!("[missing issue: {}]", dep.depends_on_id),
            status: Status::Tombstone,
            priority: Priority::MEDIUM,
            dep_type: dep.dep_type.as_str().to_string(),
        },
        Priority::MEDIUM,
        dep.created_at,
    )
}

fn dependency_metadata_from_jsonl_summary(
    dep: &Dependency,
    summaries_by_id: &HashMap<String, JsonlIssueSummary>,
    allow_external_placeholder: bool,
) -> JsonlDependencyDisplayEntry {
    if let Some(target) = summaries_by_id.get(&dep.depends_on_id) {
        return (
            target.dependency_metadata(dep.dep_type.as_str()),
            target.priority,
            target.created_at,
        );
    }

    if allow_external_placeholder && dep.depends_on_id.starts_with("external:") {
        return (
            IssueWithDependencyMetadata {
                id: dep.depends_on_id.clone(),
                title: dep
                    .depends_on_id
                    .strip_prefix("external:")
                    .unwrap_or(&dep.depends_on_id)
                    .to_string(),
                status: Status::Blocked,
                priority: Priority::MEDIUM,
                dep_type: dep.dep_type.as_str().to_string(),
            },
            Priority::MEDIUM,
            dep.created_at,
        );
    }

    (
        IssueWithDependencyMetadata {
            id: dep.depends_on_id.clone(),
            title: format!("[missing issue: {}]", dep.depends_on_id),
            status: Status::Tombstone,
            priority: Priority::MEDIUM,
            dep_type: dep.dep_type.as_str().to_string(),
        },
        Priority::MEDIUM,
        dep.created_at,
    )
}

fn find_ids_by_hash_in_memory(
    issues_by_id: &HashMap<String, Issue>,
    hash_suffix: &str,
) -> Vec<String> {
    // Use the same child-suffix-aware matching as find_matching_ids so that
    // searching for "64up6.4" matches "bd-64up6.4" consistently.
    let (search_base, search_child) = match hash_suffix.split_once('.') {
        Some((base, child)) => (base, Some(child)),
        None => (hash_suffix, None),
    };

    issues_by_id
        .keys()
        .filter(|id| {
            crate::util::id::split_prefix_remainder(id).is_some_and(|(_, remainder)| {
                let base_hash = remainder.split('.').next().unwrap_or(remainder);
                if !base_hash.contains(search_base) {
                    return false;
                }
                match search_child {
                    Some(child) => remainder
                        .split_once('.')
                        .is_some_and(|(_, candidate_child)| candidate_child == child),
                    None => true,
                }
            })
        })
        .cloned()
        .collect()
}

fn collect_external_dependency_ids(details_list: &[IssueDetails]) -> HashSet<String> {
    details_list
        .iter()
        .flat_map(|details| details.dependencies.iter().chain(details.dependents.iter()))
        .filter(|item| item.id.starts_with("external:"))
        .map(|item| item.id.clone())
        .collect()
}

fn issue_details_have_external_dependencies(details: &IssueDetails) -> bool {
    details
        .dependencies
        .iter()
        .chain(details.dependents.iter())
        .any(|item| item.id.starts_with("external:"))
}

fn apply_external_dependency_metadata(
    items: &mut [IssueWithDependencyMetadata],
    external_statuses: &HashMap<String, bool>,
) {
    for item in items {
        if !item.id.starts_with("external:") {
            continue;
        }

        let satisfied = external_statuses.get(&item.id).copied().unwrap_or(false);
        item.status = if satisfied {
            crate::model::Status::Closed
        } else {
            crate::model::Status::Blocked
        };

        let placeholder_title = item.id.strip_prefix("external:").unwrap_or(&item.id);
        if item.title.is_empty() || item.title == placeholder_title {
            item.title = format_external_dependency_title(&item.id, satisfied);
        }
    }
}

fn format_external_dependency_title(dep_id: &str, satisfied: bool) -> String {
    let prefix = if satisfied { "✓" } else { "⏳" };
    parse_external_dep_id(dep_id).map_or_else(
        || format!("{prefix} {dep_id}"),
        |(project, capability)| format!("{prefix} {project}:{capability}"),
    )
}

fn parse_external_dep_id(dep_id: &str) -> Option<(String, String)> {
    let mut parts = dep_id.splitn(3, ':');
    let prefix = parts.next()?;
    if prefix != "external" {
        return None;
    }
    let project = parts.next()?.to_string();
    let capability = parts.next()?.to_string();
    if project.is_empty() || capability.is_empty() {
        return None;
    }
    Some((project, capability))
}

fn print_issue_details(details: &IssueDetails, use_color: bool, wrap: bool) {
    let output = format_issue_details(details, use_color, wrap);
    print!("{output}");
}

/// Width used to soft-wrap free-text bodies in the non-Rich (piped / no-TTY)
/// `br show --format text` output. Honors `$COLUMNS` when set to a sane value,
/// otherwise falls back to a readable 100 columns (#370). The Rich panel path
/// uses the live terminal width instead.
fn compact_wrap_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w >= 20)
        .unwrap_or(100)
}

/// Soft-wrap `text` to `width` columns on word boundaries (#370). Existing line
/// breaks are preserved, a line's leading indentation is carried onto wrapped
/// continuations, and a single over-long word is never split. Passing
/// `usize::MAX` is a no-op, so callers can disable wrapping cheaply.
fn wrap_body(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(1);
    let mut out = String::new();
    for (line_idx, line) in text.split('\n').enumerate() {
        if line_idx > 0 {
            out.push('\n');
        }
        if UnicodeWidthStr::width(line) <= width {
            out.push_str(line);
            continue;
        }
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let avail = width.saturating_sub(UnicodeWidthStr::width(indent)).max(1);
        out.push_str(indent);
        let mut cur_w = 0usize;
        let mut started = false;
        for word in line[indent_len..].split(' ') {
            let word_w = UnicodeWidthStr::width(word);
            if !started {
                out.push_str(word);
                cur_w = word_w;
                started = true;
            } else if cur_w > 0 && cur_w + 1 + word_w > avail {
                out.push('\n');
                out.push_str(indent);
                out.push_str(word);
                cur_w = word_w;
            } else {
                out.push(' ');
                out.push_str(word);
                cur_w += 1 + word_w;
            }
        }
    }
    out
}

#[allow(clippy::too_many_lines)]
fn format_issue_details(details: &IssueDetails, use_color: bool, wrap: bool) -> String {
    let mut output = String::new();
    // `usize::MAX` makes `wrap_body` a no-op, so `--no-wrap` (or any caller that
    // opts out) keeps the exact pre-#370 unwrapped output.
    let wrap_width = if wrap {
        compact_wrap_width()
    } else {
        usize::MAX
    };
    let issue = &details.issue;
    let status_icon = format_status_icon_colored(&issue.status, use_color);
    let priority_label = format_priority_label(&issue.priority, use_color);
    let status_upper = format_status_label(&issue.status, false).to_uppercase();
    let issue_id = sanitize_terminal_inline(&issue.id);
    let title = sanitize_terminal_inline(&issue.title);

    // Match bd format: {status_icon} {id} · {title}   [● {priority} · {STATUS}]
    let _ = writeln!(
        output,
        "{} {} · {}   [● {} · {}]",
        status_icon, issue_id, title, priority_label, status_upper
    );

    // Owner/Type line: Owner: {owner} · Type: {type}
    let owner = issue
        .owner
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()));
    let owner = sanitize_terminal_inline(&owner);
    let _ = writeln!(
        output,
        "Owner: {} · Type: {}",
        owner,
        format_type_label(&issue.issue_type)
    );

    // Created/Updated line
    let _ = writeln!(
        output,
        "Created: {} · Updated: {}",
        issue.created_at.format("%Y-%m-%d"),
        issue.updated_at.format("%Y-%m-%d")
    );

    if let Some(assignee) = &issue.assignee {
        let _ = writeln!(output, "Assignee: {}", sanitize_terminal_inline(assignee));
    }

    if !details.labels.is_empty() {
        let labels = details
            .labels
            .iter()
            .map(|label| sanitize_terminal_inline(label).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(output, "Labels: {labels}");
    }

    if let Some(ext_ref) = &issue.external_ref
        && !ext_ref.is_empty()
    {
        let _ = writeln!(output, "Ref: {}", sanitize_terminal_inline(ext_ref));
    }

    if let Some(due) = &issue.due_at {
        let _ = writeln!(output, "Due: {}", due.format("%Y-%m-%d"));
    }

    if let Some(defer) = &issue.defer_until {
        let _ = writeln!(output, "Deferred until: {}", defer.format("%Y-%m-%d"));
    }

    if let Some(minutes) = issue.estimated_minutes
        && minutes > 0
    {
        let hours = minutes / 60;
        let remaining = minutes % 60;
        if hours > 0 && remaining > 0 {
            let _ = writeln!(output, "Estimate: {hours}h {remaining}m");
        } else if hours > 0 {
            let _ = writeln!(output, "Estimate: {hours}h");
        } else {
            let _ = writeln!(output, "Estimate: {remaining}m");
        }
    }

    if let Some(closed) = &issue.closed_at {
        let reason_str = issue.close_reason.as_deref().unwrap_or("closed");
        let _ = writeln!(
            output,
            "Closed: {} ({})",
            closed.format("%Y-%m-%d"),
            sanitize_terminal_inline(reason_str)
        );
    }

    if let Some(rollup) = &details.rollup {
        let breakdown = rollup
            .descendants
            .iter()
            .map(|(status, count)| format!("{count} {}", sanitize_terminal_inline(status)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            output,
            "Rollup: {} ({breakdown})",
            sanitize_terminal_inline(&rollup.status)
        );
    }

    if let Some(desc) = &issue.description {
        output.push('\n');
        let _ = writeln!(
            output,
            "{}",
            wrap_body(sanitize_terminal_text(desc).as_ref(), wrap_width)
        );
    }

    if let Some(design) = &issue.design
        && !design.is_empty()
    {
        output.push('\n');
        let _ = writeln!(output, "Design:");
        let _ = writeln!(
            output,
            "{}",
            wrap_body(sanitize_terminal_text(design).as_ref(), wrap_width)
        );
    }

    if let Some(ac) = &issue.acceptance_criteria
        && !ac.is_empty()
    {
        output.push('\n');
        let _ = writeln!(output, "Acceptance Criteria:");
        let _ = writeln!(
            output,
            "{}",
            wrap_body(sanitize_terminal_text(ac).as_ref(), wrap_width)
        );
    }

    if let Some(notes) = &issue.notes
        && !notes.is_empty()
    {
        output.push('\n');
        let _ = writeln!(output, "Notes:");
        let _ = writeln!(
            output,
            "{}",
            wrap_body(sanitize_terminal_text(notes).as_ref(), wrap_width)
        );
    }

    if !details.dependencies.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependencies:");
        for dep in &details.dependencies {
            let _ = writeln!(
                output,
                "  -> {} ({}) - {}",
                sanitize_terminal_inline(&dep.id),
                sanitize_terminal_inline(&dep.dep_type),
                sanitize_terminal_inline(&dep.title)
            );
        }
    }

    if !details.dependents.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependents:");
        for dep in &details.dependents {
            let _ = writeln!(
                output,
                "  <- {} ({}) - {}",
                sanitize_terminal_inline(&dep.id),
                sanitize_terminal_inline(&dep.dep_type),
                sanitize_terminal_inline(&dep.title)
            );
        }
    }

    if !details.comments.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Comments:");
        for comment in &details.comments {
            let _ = writeln!(
                output,
                "  [{}] {}: {}",
                comment.created_at.format("%Y-%m-%d %H:%M UTC"),
                sanitize_terminal_inline(&comment.author),
                wrap_body(sanitize_terminal_text(&comment.body).as_ref(), wrap_width)
            );
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{
        apply_external_dependency_metadata, build_issue_details_from_jsonl, format_issue_details,
        load_issue_details_from_jsonl, reorder_details_by_requested_inputs,
    };
    use crate::format::{IssueDetails, IssueWithDependencyMetadata};
    use crate::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use crate::util::id::{IdResolver, ResolverConfig};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use std::io::Write;
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: Some("Test description".to_string()),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
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

    #[test]
    fn test_show_retrieves_issue_by_id() {
        init_logging();
        info!("test_show_retrieves_issue_by_id: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-001", "Test Issue");
        storage.create_issue(&issue, "tester").unwrap();

        let retrieved = storage.get_issue("bd-001").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "bd-001");
        assert_eq!(retrieved.title, "Test Issue");
        info!("test_show_retrieves_issue_by_id: assertions passed");
    }

    #[test]
    fn test_show_returns_none_for_missing_id() {
        init_logging();
        info!("test_show_returns_none_for_missing_id: starting");
        let storage = SqliteStorage::open_memory().unwrap();

        let retrieved = storage.get_issue("nonexistent").unwrap();
        assert!(retrieved.is_none());
        info!("test_show_returns_none_for_missing_id: assertions passed");
    }

    #[test]
    fn test_show_multiple_issues() {
        init_logging();
        info!("test_show_multiple_issues: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "First Issue");
        let issue2 = make_test_issue("bd-002", "Second Issue");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        let retrieved1 = storage.get_issue("bd-001").unwrap().unwrap();
        let retrieved2 = storage.get_issue("bd-002").unwrap().unwrap();

        assert_eq!(retrieved1.title, "First Issue");
        assert_eq!(retrieved2.title, "Second Issue");
        info!("test_show_multiple_issues: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization() {
        init_logging();
        info!("test_issue_json_serialization: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string_pretty(&issue).unwrap();

        assert!(json.contains("\"id\": \"bd-001\""));
        assert!(json.contains("\"title\": \"Test Issue\""));
        assert!(json.contains("\"status\": \"open\""));
        info!("test_issue_json_serialization: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization_multiple() {
        init_logging();
        info!("test_issue_json_serialization_multiple: starting");
        let issues = vec![
            make_test_issue("bd-001", "First"),
            make_test_issue("bd-002", "Second"),
        ];

        let json = serde_json::to_string_pretty(&issues).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "bd-001");
        assert_eq!(parsed[1]["id"], "bd-002");
        info!("test_issue_json_serialization_multiple: assertions passed");
    }

    #[test]
    fn test_show_resolves_full_id() {
        init_logging();
        info!("test_show_resolves_full_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve("bd-abc123", |id| id == "bd-abc123", |_hash| Vec::new())
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_full_id: assertions passed");
    }

    #[test]
    fn test_show_resolves_prefixed_id() {
        init_logging();
        info!("test_show_resolves_prefixed_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve("abc123", |id| id == "bd-abc123", |_hash| Vec::new())
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_prefixed_id: assertions passed");
    }

    #[test]
    fn test_show_resolves_partial_id() {
        init_logging();
        info!("test_show_resolves_partial_id: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let resolved_id = resolver
            .resolve(
                "abc",
                |_id| false,
                |hash| {
                    if hash == "abc" {
                        vec!["bd-abc123".to_string()]
                    } else {
                        Vec::new()
                    }
                },
            )
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_partial_id: assertions passed");
    }

    #[test]
    fn test_show_not_found_error() {
        init_logging();
        info!("test_show_not_found_error: starting");
        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let result = resolver.resolve("missing", |_id| false, |_hash| Vec::new());
        assert!(result.is_err());
        info!("test_show_not_found_error: assertions passed");
    }

    #[test]
    fn test_show_json_output_shape() {
        init_logging();
        info!("test_show_json_output_shape: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let details = IssueDetails {
            issue: issue.clone(),
            labels: vec!["bug".to_string()],
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&vec![details]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], issue.id);
        assert!(parsed[0]["labels"].is_array());
        assert!(parsed[0]["dependencies"].is_array());
        info!("test_show_json_output_shape: assertions passed");
    }

    #[test]
    fn test_show_text_includes_dependencies_and_comments() {
        init_logging();
        info!("test_show_text_includes_dependencies_and_comments: starting");
        let mut issue = make_test_issue("bd-001", "Test Issue");
        issue.description = None;
        let details = IssueDetails {
            issue,
            labels: Vec::new(),
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: vec![Comment {
                id: 1,
                issue_id: "bd-001".to_string(),
                author: "alice".to_string(),
                body: "Looks good".to_string(),
                created_at: Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 0).unwrap(),
            }],
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };
        let output = format_issue_details(&details, false, false);
        assert!(output.contains("Dependencies:"));
        assert!(output.contains("-> bd-002 (blocks) - Dep"));
        assert!(output.contains("Comments:"));
        assert!(output.contains("alice: Looks good"));
        info!("test_show_text_includes_dependencies_and_comments: assertions passed");
    }

    #[test]
    fn test_show_text_sanitizes_ids_and_dependency_types() {
        init_logging();
        info!("test_show_text_sanitizes_ids_and_dependency_types: starting");
        let mut issue = make_test_issue("bd-001\x1b]52;c;bad\x07", "Test Issue");
        issue.description = None;
        let details = IssueDetails {
            issue,
            labels: Vec::new(),
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002\x1b[2J".to_string(),
                title: "Dep\x07".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks\x1b[31m".to_string(),
            }],
            dependents: vec![IssueWithDependencyMetadata {
                id: "bd-003".to_string(),
                title: "Dependent".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "waits-for\x07".to_string(),
            }],
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };

        let output = format_issue_details(&details, false, false);

        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("bd-001\\u{1b}]52;c;bad\\u{7}"));
        assert!(output.contains("bd-002\\u{1b}[2J"));
        assert!(output.contains("(blocks\\u{1b}[31m)"));
        assert!(output.contains("(waits-for\\u{7})"));
        assert!(output.contains("Dep\\u{7}"));
        info!("test_show_text_sanitizes_ids_and_dependency_types: assertions passed");
    }

    #[test]
    fn test_apply_external_dependency_metadata_updates_generated_placeholder() {
        init_logging();
        info!("test_apply_external_dependency_metadata_updates_generated_placeholder: starting");
        let mut dependencies = vec![IssueWithDependencyMetadata {
            id: "external:proj:cap".to_string(),
            title: "proj:cap".to_string(),
            status: Status::Blocked,
            priority: Priority::MEDIUM,
            dep_type: "blocks".to_string(),
        }];

        let mut statuses = HashMap::new();
        statuses.insert("external:proj:cap".to_string(), true);

        apply_external_dependency_metadata(&mut dependencies, &statuses);

        assert_eq!(dependencies[0].status, Status::Closed);
        assert_eq!(dependencies[0].title, "✓ proj:cap");
        info!(
            "test_apply_external_dependency_metadata_updates_generated_placeholder: assertions passed"
        );
    }

    #[test]
    fn test_build_issue_details_from_jsonl_derives_parent_and_dependents() {
        init_logging();
        info!("test_build_issue_details_from_jsonl_derives_parent_and_dependents: starting");

        let mut parent = make_test_issue("bd-parent", "Parent");
        parent.priority = Priority::HIGH;

        let mut child = make_test_issue("bd-child", "Child");
        child.labels = vec!["backend".to_string()];
        child.comments = vec![Comment {
            id: 7,
            issue_id: "bd-child".to_string(),
            author: "alice".to_string(),
            body: "Investigating".to_string(),
            created_at: Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 0).unwrap(),
        }];
        child.dependencies = vec![Dependency {
            issue_id: "bd-child".to_string(),
            depends_on_id: "bd-parent".to_string(),
            dep_type: DependencyType::ParentChild,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 1, 0, 0).unwrap(),
            created_by: Some("tester".to_string()),
            metadata: None,
            thread_id: None,
        }];

        let issues_by_id = HashMap::from([
            (parent.id.clone(), parent.clone()),
            (child.id.clone(), child.clone()),
        ]);

        let child_details = build_issue_details_from_jsonl(&child, &issues_by_id).unwrap();
        assert_eq!(child_details.parent.as_deref(), Some("bd-parent"));
        assert_eq!(child_details.labels, vec!["backend".to_string()]);
        assert_eq!(child_details.comments.len(), 1);
        assert!(child_details.issue.labels.is_empty());
        assert!(child_details.issue.dependencies.is_empty());
        assert!(child_details.issue.comments.is_empty());

        let parent_details = build_issue_details_from_jsonl(&parent, &issues_by_id).unwrap();
        assert_eq!(parent_details.dependents.len(), 1);
        assert_eq!(parent_details.dependents[0].id, "bd-child");
        assert_eq!(parent_details.dependents[0].dep_type, "parent-child");
        info!(
            "test_build_issue_details_from_jsonl_derives_parent_and_dependents: assertions passed"
        );
    }

    #[test]
    fn test_load_issue_details_from_jsonl_exact_streaming_matches_materialized() {
        init_logging();
        info!("test_load_issue_details_from_jsonl_exact_streaming_matches_materialized: starting");

        let mut dependency = make_test_issue("bd-dep01", "Dependency");
        dependency.priority = Priority::HIGH;

        let mut target = make_test_issue("bd-target", "Target");
        target.dependencies.push(Dependency {
            issue_id: target.id.clone(),
            depends_on_id: dependency.id.clone(),
            dep_type: DependencyType::Blocks,
            created_at: target.created_at,
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        let mut dependent = make_test_issue("bd-dependent", "Dependent");
        dependent.priority = Priority::CRITICAL;
        dependent.dependencies.push(Dependency {
            issue_id: dependent.id.clone(),
            depends_on_id: target.id.clone(),
            dep_type: DependencyType::Related,
            created_at: dependent.created_at,
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        let issues_by_id = HashMap::from([
            (dependency.id.clone(), dependency.clone()),
            (target.id.clone(), target.clone()),
            (dependent.id.clone(), dependent.clone()),
        ]);
        let expected = build_issue_details_from_jsonl(&target, &issues_by_id).unwrap();

        let mut jsonl = tempfile::NamedTempFile::new().unwrap();
        for issue in [&dependency, &target, &dependent] {
            writeln!(jsonl, "{}", serde_json::to_string(issue).unwrap()).unwrap();
        }

        let resolver = IdResolver::new(ResolverConfig::with_prefix("bd"));
        let actual = load_issue_details_from_jsonl(
            &[target.id.clone()],
            &resolver,
            jsonl.path(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(vec![expected]).unwrap()
        );
        info!(
            "test_load_issue_details_from_jsonl_exact_streaming_matches_materialized: assertions passed"
        );
    }

    #[test]
    fn test_exact_jsonl_show_capacity_is_capped_for_sparse_or_huge_files() {
        assert_eq!(super::estimated_jsonl_show_capacity(0), 0);
        assert_eq!(super::estimated_jsonl_show_capacity(12_500), 25);
        assert_eq!(
            super::estimated_jsonl_show_capacity(u64::MAX),
            super::MAX_EXACT_JSONL_SHOW_INITIAL_CAPACITY
        );
    }

    #[test]
    fn test_build_issue_details_from_jsonl_preserves_missing_dependency_placeholder() {
        init_logging();
        info!(
            "test_build_issue_details_from_jsonl_preserves_missing_dependency_placeholder: starting"
        );

        let mut issue = make_test_issue("bd-root", "Root");
        issue.dependencies = vec![Dependency {
            issue_id: "bd-root".to_string(),
            depends_on_id: "bd-missing".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 1, 0, 0).unwrap(),
            created_by: Some("tester".to_string()),
            metadata: None,
            thread_id: None,
        }];

        let issues_by_id = HashMap::from([(issue.id.clone(), issue.clone())]);
        let details = build_issue_details_from_jsonl(&issue, &issues_by_id).unwrap();

        assert_eq!(details.dependencies.len(), 1);
        assert_eq!(details.dependencies[0].id, "bd-missing");
        assert_eq!(details.dependencies[0].title, "[missing issue: bd-missing]");
        assert_eq!(details.dependencies[0].status, Status::Tombstone);
        assert_eq!(details.dependencies[0].priority, Priority::MEDIUM);
        info!(
            "test_build_issue_details_from_jsonl_preserves_missing_dependency_placeholder: assertions passed"
        );
    }

    #[test]
    fn test_reorder_details_by_requested_inputs_restores_mixed_route_order() {
        init_logging();
        info!("test_reorder_details_by_requested_inputs_restores_mixed_route_order: starting");

        let local_first = IssueDetails {
            issue: make_test_issue("bd-local-1", "Local first"),
            labels: Vec::new(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };
        let local_last = IssueDetails {
            issue: make_test_issue("bd-local-2", "Local last"),
            labels: Vec::new(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };
        let external_middle = IssueDetails {
            issue: make_test_issue("ext-middle", "External middle"),
            labels: Vec::new(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            rollup: None,
            inherited_context: Vec::new(),
        };

        let ordered = reorder_details_by_requested_inputs(
            &[
                "bd-local-1".to_string(),
                "ext-middle".to_string(),
                "bd-local-2".to_string(),
            ],
            vec![
                (
                    vec!["bd-local-1".to_string(), "bd-local-2".to_string()],
                    vec![local_first, local_last],
                ),
                (vec!["ext-middle".to_string()], vec![external_middle]),
            ],
        )
        .unwrap();

        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].issue.id, "bd-local-1");
        assert_eq!(ordered[1].issue.id, "ext-middle");
        assert_eq!(ordered[2].issue.id, "bd-local-2");
        info!(
            "test_reorder_details_by_requested_inputs_restores_mixed_route_order: assertions passed"
        );
    }

    #[test]
    fn routed_structured_output_guard_excludes_text_and_csv() {
        use crate::cli::OutputFormat;

        let structured_formats = [OutputFormat::Json, OutputFormat::Toon];
        let text_formats = [OutputFormat::Text, OutputFormat::Csv];

        for fmt in &structured_formats {
            assert!(
                matches!(fmt, OutputFormat::Json | OutputFormat::Toon),
                "{fmt:?} should pass the structured-output guard"
            );
        }
        for fmt in &text_formats {
            assert!(
                !matches!(fmt, OutputFormat::Json | OutputFormat::Toon),
                "{fmt:?} should NOT pass the structured-output guard"
            );
        }
    }
}
