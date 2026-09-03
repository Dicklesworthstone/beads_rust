//! Dependency command implementation.

use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, cli_for_routed_workspace,
    external_project_db_paths_after_auto_import_if_needed, finalize_batched_blocked_cache_refresh,
    report_auto_flush_failure, resolve_issue_id, retry_mutation_with_jsonl_recovery,
};
use crate::cli::{
    DepAddArgs, DepCommands, DepCyclesArgs, DepDirection, DepImportArgs, DepListArgs,
    DepRemoveArgs, DepTreeArgs, OutputFormat, resolve_output_format_basic_with_outer_mode,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{sanitize_terminal_inline, truncate_title};
use crate::model::DependencyType;
use crate::output::{OutputContext, OutputMode, Theme};
use crate::storage::{BulkDependencyInsert, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Execute the dep command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(
    command: &DepCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    match command {
        DepCommands::Add(args) => execute_dep_add(args, json, cli, ctx, &beads_dir),
        DepCommands::Import(args) => execute_dep_import(args, json, cli, ctx, &beads_dir),
        DepCommands::Remove(args) => execute_dep_remove(args, json, cli, ctx, &beads_dir),
        DepCommands::List(args) => execute_dep_list(args, cli, ctx, &beads_dir),
        DepCommands::Tree(args) => execute_dep_tree(args, json, cli, ctx, &beads_dir),
        DepCommands::Cycles(args) => {
            let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
            dep_cycles(args, &storage_ctx.storage, json, ctx)
        }
    }
}

/// Execute a read-only dep command using storage that was already opened by the caller.
///
/// Returns `Ok(false)` when the command needs the normal routed or mutating path.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute_with_storage_ctx(
    command: &DepCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    match command {
        DepCommands::List(args) => {
            execute_local_dep_list_with_storage_ctx(args, cli, ctx, local_beads_dir, storage_ctx)
        }
        DepCommands::Tree(args) => {
            execute_local_dep_tree_with_storage_ctx(args, cli, ctx, local_beads_dir, storage_ctx)
        }
        DepCommands::Cycles(args) => {
            dep_cycles(args, &storage_ctx.storage, json, ctx)?;
            Ok(true)
        }
        DepCommands::Add(_) | DepCommands::Import(_) | DepCommands::Remove(_) => Ok(false),
    }
}

fn execute_dep_add(
    args: &DepAddArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
) -> Result<()> {
    validate_dependency_target_route(local_beads_dir, &args.issue, &args.depends_on)?;
    let (mut storage_ctx, route_cli, auto_flush_external, _routed_write_lock) =
        open_routed_storage_for_input(local_beads_dir, cli, &args.issue)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let actor = config::resolve_actor(&config_layer);
    dep_add(
        args,
        &mut storage_ctx,
        &resolver,
        &actor,
        ctx,
        local_beads_dir,
        auto_flush_external,
    )
}

fn execute_dep_remove(
    args: &DepRemoveArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
) -> Result<()> {
    validate_dependency_target_route(local_beads_dir, &args.issue, &args.depends_on)?;
    let (mut storage_ctx, route_cli, auto_flush_external, _routed_write_lock) =
        open_routed_storage_for_input(local_beads_dir, cli, &args.issue)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let actor = config::resolve_actor(&config_layer);
    dep_remove(
        args,
        &mut storage_ctx,
        &resolver,
        &actor,
        ctx,
        local_beads_dir,
        auto_flush_external,
    )
}

fn execute_dep_import(
    args: &DepImportArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
) -> Result<()> {
    let mut storage_ctx = config::open_storage_with_cli(local_beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let dependencies = read_dependency_imports(&args.path)?;
    dep_import(args, &dependencies, &mut storage_ctx, &actor, ctx)
}

fn execute_dep_list(
    args: &DepListArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
) -> Result<()> {
    let (storage_ctx, route_cli, _, _routed_write_lock) =
        open_routed_storage_for_input(local_beads_dir, cli, &args.issue)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let use_color = config::should_use_color(&config_layer);
    let quiet = route_cli.quiet.unwrap_or(false);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
        &storage_ctx.storage,
        &config_layer,
        &storage_ctx.paths.beads_dir,
        &route_cli,
    )?;

    dep_list(
        args,
        &storage_ctx.storage,
        &resolver,
        &external_db_paths,
        ctx,
        quiet,
        !use_color,
    )
}

fn execute_local_dep_list_with_storage_ctx(
    args: &DepListArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    if config::routing::resolve_route(&args.issue, local_beads_dir)?.is_external {
        return Ok(false);
    }

    let config_layer = storage_ctx.load_config(cli)?;
    let use_color = config::should_use_color(&config_layer);
    let quiet = cli.quiet.unwrap_or(false);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
        &storage_ctx.storage,
        &config_layer,
        &storage_ctx.paths.beads_dir,
        cli,
    )?;

    dep_list(
        args,
        &storage_ctx.storage,
        &resolver,
        &external_db_paths,
        ctx,
        quiet,
        !use_color,
    )?;
    Ok(true)
}

fn execute_dep_tree(
    args: &DepTreeArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
) -> Result<()> {
    let (storage_ctx, route_cli, _, _routed_write_lock) =
        open_routed_storage_for_input(local_beads_dir, cli, &args.issue)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
        &storage_ctx.storage,
        &config_layer,
        &storage_ctx.paths.beads_dir,
        &route_cli,
    )?;

    dep_tree(
        args,
        &storage_ctx.storage,
        &resolver,
        &external_db_paths,
        false,
        ctx,
    )
}

fn execute_local_dep_tree_with_storage_ctx(
    args: &DepTreeArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    if config::routing::resolve_route(&args.issue, local_beads_dir)?.is_external {
        return Ok(false);
    }

    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let external_db_paths = external_project_db_paths_after_auto_import_if_needed(
        &storage_ctx.storage,
        &config_layer,
        &storage_ctx.paths.beads_dir,
        cli,
    )?;

    dep_tree(
        args,
        &storage_ctx.storage,
        &resolver,
        &external_db_paths,
        false,
        ctx,
    )?;
    Ok(true)
}

fn open_routed_storage_for_input(
    local_beads_dir: &Path,
    cli: &config::CliOverrides,
    issue_input: &str,
) -> Result<(
    config::OpenStorageResult,
    config::CliOverrides,
    bool,
    RoutedWorkspaceWriteLock,
)> {
    let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
    let mut route_cli = cli_for_routed_workspace(cli, route.is_external);
    let routed_write_lock = acquire_routed_workspace_write_lock(
        &route.beads_dir,
        route.is_external,
        route_cli.lock_timeout,
    )?;
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli)?;
    Ok((storage_ctx, route_cli, route.is_external, routed_write_lock))
}

fn validate_dependency_target_route(
    local_beads_dir: &Path,
    issue_input: &str,
    depends_on_input: &str,
) -> Result<()> {
    if depends_on_input.starts_with("external:") {
        return Ok(());
    }

    let issue_route = config::routing::resolve_route(issue_input, local_beads_dir)?;
    let depends_on_route = config::routing::resolve_route(depends_on_input, local_beads_dir)?;

    if issue_route.beads_dir == depends_on_route.beads_dir {
        return Ok(());
    }

    Err(BeadsError::validation(
        "depends_on",
        format!(
            "issue '{issue_input}' and dependency target '{depends_on_input}' resolve to different projects; use an explicit external:... dependency for cross-project links"
        ),
    ))
}

/// JSON output for dep add/remove operations
#[derive(Serialize)]
struct DepActionResult {
    status: String,
    issue_id: String,
    depends_on_id: String,
    #[serde(rename = "type")]
    dep_type: String,
    action: String,
}

/// JSON output for dep import operations.
#[derive(Serialize)]
struct DepImportResult {
    status: String,
    input_path: String,
    imported: usize,
    skipped: usize,
    total_edges: usize,
    action: String,
}

#[derive(Debug, Deserialize)]
struct DepImportLine {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    issue_id: Option<String>,
    #[serde(default)]
    depends_on_id: Option<String>,
    #[serde(default, rename = "type", alias = "dep_type")]
    dep_type: Option<String>,
    #[serde(default)]
    dependencies: Vec<DepImportNestedDependency>,
}

#[derive(Debug, Deserialize)]
struct DepImportNestedDependency {
    #[serde(default)]
    issue_id: Option<String>,
    depends_on_id: String,
    #[serde(default, rename = "type", alias = "dep_type")]
    dep_type: Option<String>,
}

fn read_dependency_imports(path: &Path) -> Result<Vec<BulkDependencyInsert>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut dependencies = Vec::new();

    for (line_index, line) in reader.lines().enumerate() {
        let line_number = line_index + 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        dependencies.extend(parse_dependency_import_line(&line, line_number)?);
    }

    if dependencies.is_empty() {
        return Err(BeadsError::validation(
            "path",
            format!(
                "dependency import file '{}' did not contain any dependency edges",
                path.display()
            ),
        ));
    }

    Ok(dependencies)
}

fn parse_dependency_import_line(
    line: &str,
    line_number: usize,
) -> Result<Vec<BulkDependencyInsert>> {
    let record: DepImportLine = serde_json::from_str(line).map_err(|err| {
        BeadsError::validation(
            "jsonl",
            format!("invalid dependency JSONL at line {line_number}: {err}"),
        )
    })?;

    let mut dependencies = Vec::new();

    if let (Some(issue_id), Some(depends_on_id)) = (&record.issue_id, &record.depends_on_id) {
        dependencies.push(build_bulk_dependency_insert(
            issue_id,
            depends_on_id,
            record.dep_type.as_deref(),
            line_number,
        )?);
    }

    if !record.dependencies.is_empty() {
        let parent_issue_id = record.id.as_deref().or(record.issue_id.as_deref()).ok_or_else(|| {
            BeadsError::validation(
                "issue_id",
                format!(
                    "dependency JSONL line {line_number} has a dependencies array but no id or issue_id"
                ),
            )
        })?;

        for dep in &record.dependencies {
            let issue_id = dep.issue_id.as_deref().unwrap_or(parent_issue_id);
            dependencies.push(build_bulk_dependency_insert(
                issue_id,
                &dep.depends_on_id,
                dep.dep_type.as_deref(),
                line_number,
            )?);
        }
    }

    if dependencies.is_empty() {
        if record.id.is_some() {
            return Ok(dependencies);
        }

        return Err(BeadsError::validation(
            "jsonl",
            format!(
                "dependency JSONL line {line_number} must contain either issue_id + depends_on_id or an issue record with dependencies"
            ),
        ));
    }

    Ok(dependencies)
}

fn build_bulk_dependency_insert(
    issue_id: &str,
    depends_on_id: &str,
    dep_type: Option<&str>,
    line_number: usize,
) -> Result<BulkDependencyInsert> {
    let issue_id = issue_id.trim();
    let depends_on_id = depends_on_id.trim();
    if issue_id.is_empty() || depends_on_id.is_empty() {
        return Err(BeadsError::validation(
            "jsonl",
            format!("dependency JSONL line {line_number} contains an empty issue id"),
        ));
    }

    let dep_type = dep_type.unwrap_or("blocks").trim();
    let dep_type = parse_dependency_type(dep_type)
        .map_err(|err| BeadsError::WithContext {
            context: format!("dependency JSONL line {line_number} has an invalid type"),
            source: Box::new(err),
        })?
        .as_str()
        .to_string();

    Ok(BulkDependencyInsert {
        issue_id: issue_id.to_string(),
        depends_on_id: depends_on_id.to_string(),
        dep_type,
    })
}

fn finalize_dep_mutation(
    storage_ctx: &mut config::OpenStorageResult,
    cache_dirty: bool,
    command: &str,
) -> Result<()> {
    finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, command)?;
    storage_ctx.flush_no_db_if_dirty()
}

/// JSON output for dep list
#[derive(Serialize)]
struct DepListItem {
    issue_id: String,
    depends_on_id: String,
    #[serde(rename = "type")]
    dep_type: String,
    title: String,
    status: String,
    priority: i32,
}

/// JSON output for dep tree
#[derive(Serialize)]
struct TreeNode {
    #[serde(skip_serializing)]
    node_key: String,
    id: String,
    title: String,
    depth: usize,
    parent_id: Option<String>,
    #[serde(skip_serializing)]
    parent_key: Option<String>,
    priority: i32,
    status: String,
    truncated: bool,
    /// This occurrence's subtree was elided because the same issue was already
    /// expanded elsewhere in the tree (GitHub #392).
    ///
    /// Distinct from `truncated`, which means "children exist but `--max-depth`
    /// stopped us". A `repeat` node is reachable through more than one parent
    /// (a diamond); it is still listed under every parent, but only its first
    /// occurrence carries the expanded subtree.
    repeat: bool,
}

/// JSON output for dep cycles
#[derive(Serialize)]
struct CyclesResult {
    cycles: Vec<Vec<String>>,
    count: usize,
    active_count: usize,
    archived_closed_count: usize,
    total_count: usize,
    blocking_only: bool,
    include_closed: bool,
    scope: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    active_cycles: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    archived_closed_cycles: Vec<Vec<String>>,
}

fn dep_add(
    args: &DepAddArgs,
    storage_ctx: &mut config::OpenStorageResult,
    resolver: &IdResolver,
    actor: &str,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(&storage_ctx.storage, resolver, &args.issue)?;

    // External dependencies don't need resolution
    let depends_on_id = if args.depends_on.starts_with("external:") {
        args.depends_on.clone()
    } else {
        resolve_issue_id(&storage_ctx.storage, resolver, &args.depends_on)?
    };

    let dep_type = parse_dependency_type(&args.dep_type)?;

    // Self-dependency check
    if issue_id == depends_on_id {
        return Err(BeadsError::SelfDependency { id: issue_id });
    }

    let added = retry_mutation_with_jsonl_recovery(
        storage_ctx,
        true,
        "dep add",
        Some(issue_id.as_str()),
        |storage| {
            storage.add_dependency_with_metadata(
                &issue_id,
                &depends_on_id,
                dep_type.as_str(),
                actor,
                args.metadata.as_deref(),
            )
        },
    )?;

    finalize_dep_mutation(storage_ctx, added, "dep add")?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }
    crate::util::set_last_touched_id(local_beads_dir, &issue_id);

    if ctx.is_json() || ctx.is_toon() {
        let result = DepActionResult {
            status: if added { "ok" } else { "exists" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type: dep_type.as_str().to_string(),
            action: if added { "added" } else { "already_exists" }.to_string(),
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else if added {
        let issue_id_display = dep_display_text(&issue_id);
        let depends_on_id_display = dep_display_text(&depends_on_id);
        let dep_type_display = dep_display_text(dep_type.as_str());
        if ctx.is_rich() {
            // Rich mode: Show detailed visual feedback
            ctx.success(&format!(
                "Added dependency: {} → {}",
                issue_id_display, depends_on_id_display
            ));
            let relationship = match dep_type {
                DependencyType::Blocks => format!(
                    "  {} now blocks {}",
                    depends_on_id_display, issue_id_display
                ),
                DependencyType::ParentChild => {
                    format!(
                        "  {} is parent of {}",
                        depends_on_id_display, issue_id_display
                    )
                }
                DependencyType::WaitsFor => {
                    format!("  {} waits for {}", issue_id_display, depends_on_id_display)
                }
                _ => format!("  Relationship: {}", dep_type_display),
            };
            ctx.print_line(&relationship);
        } else {
            ctx.success(&format!(
                "Added dependency: {} -> {} ({})",
                issue_id_display, depends_on_id_display, dep_type_display
            ));
        }
    } else {
        let issue_id_display = dep_display_text(&issue_id);
        let depends_on_id_display = dep_display_text(&depends_on_id);
        ctx.info(&format!(
            "Dependency already exists: {issue_id_display} → {depends_on_id_display}"
        ));
    }

    Ok(())
}

fn dep_import(
    args: &DepImportArgs,
    dependencies: &[BulkDependencyInsert],
    storage_ctx: &mut config::OpenStorageResult,
    actor: &str,
    ctx: &OutputContext,
) -> Result<()> {
    let total_edges = dependencies.len();
    let probe_issue_id = dependencies.first().map(|dep| dep.issue_id.as_str());
    let imported = retry_mutation_with_jsonl_recovery(
        storage_ctx,
        true,
        "dep import",
        probe_issue_id,
        |storage| storage.add_dependencies_bulk_for_import(dependencies, actor),
    )?;

    finalize_dep_mutation(storage_ctx, imported > 0, "dep import")?;
    if let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    if ctx.is_json() || ctx.is_toon() {
        let result = DepImportResult {
            status: "ok".to_string(),
            input_path: args.path.display().to_string(),
            imported,
            skipped: total_edges.saturating_sub(imported),
            total_edges,
            action: "imported".to_string(),
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else {
        ctx.success(&format!(
            "Imported {} dependencies from {} ({} skipped)",
            imported,
            dep_display_text(&args.path.display().to_string()),
            total_edges.saturating_sub(imported)
        ));
    }

    Ok(())
}

fn dep_remove(
    args: &DepRemoveArgs,
    storage_ctx: &mut config::OpenStorageResult,
    resolver: &IdResolver,
    actor: &str,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(&storage_ctx.storage, resolver, &args.issue)?;

    // External dependencies don't need resolution
    let depends_on_id = if args.depends_on.starts_with("external:") {
        args.depends_on.clone()
    } else {
        resolve_issue_id(&storage_ctx.storage, resolver, &args.depends_on)?
    };

    let dep_type = dependency_type_for_pair(&storage_ctx.storage, &issue_id, &depends_on_id)?
        .unwrap_or_else(|| "unknown".to_string());
    let removed = retry_mutation_with_jsonl_recovery(
        storage_ctx,
        true,
        "dep remove",
        Some(issue_id.as_str()),
        |storage| storage.remove_dependency(&issue_id, &depends_on_id, actor),
    )?;

    finalize_dep_mutation(storage_ctx, removed, "dep remove")?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }
    crate::util::set_last_touched_id(local_beads_dir, &issue_id);

    if ctx.is_json() || ctx.is_toon() {
        let result = DepActionResult {
            status: if removed { "ok" } else { "not_found" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type,
            action: if removed { "removed" } else { "not_found" }.to_string(),
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else if removed {
        let issue_id_display = dep_display_text(&issue_id);
        let depends_on_id_display = dep_display_text(&depends_on_id);
        if ctx.is_rich() {
            ctx.success(&format!(
                "Removed dependency: {} → {}",
                issue_id_display, depends_on_id_display
            ));
            ctx.print_line(&format!(
                "  {} no longer depends on {}",
                issue_id_display, depends_on_id_display
            ));
        } else {
            ctx.success(&format!(
                "Removed dependency: {issue_id_display} -> {depends_on_id_display}"
            ));
        }
    } else {
        let issue_id_display = dep_display_text(&issue_id);
        let depends_on_id_display = dep_display_text(&depends_on_id);
        ctx.warning(&format!(
            "Dependency not found: {issue_id_display} → {depends_on_id_display}"
        ));
    }

    Ok(())
}

fn dependency_type_for_pair(
    storage: &SqliteStorage,
    issue_id: &str,
    depends_on_id: &str,
) -> Result<Option<String>> {
    Ok(storage
        .get_dependencies_full(issue_id)?
        .into_iter()
        .find(|dep| dep.depends_on_id == depends_on_id)
        .map(|dep| dep.dep_type.as_str().to_string()))
}

fn dep_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

/// Dependency types `dep add --type` accepts, in the order hints list them.
const DEPENDENCY_TYPE_NAMES: [&str; 11] = [
    "blocks",
    "parent-child",
    "conditional-blocks",
    "waits-for",
    "related",
    "discovered-from",
    "replies-to",
    "relates-to",
    "duplicates",
    "supersedes",
    "caused-by",
];

/// Closest accepted dependency type for a mistyped `--type` value: the
/// underscore or space spelling of a real type, a unique prefix, or a name
/// within two typos.
fn nearest_dependency_type(input: &str) -> Option<&'static str> {
    let normalized = input.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    if normalized.is_empty() {
        return None;
    }
    if let Some(exact) = DEPENDENCY_TYPE_NAMES
        .iter()
        .copied()
        .find(|name| *name == normalized)
    {
        return Some(exact);
    }
    let mut prefixed = DEPENDENCY_TYPE_NAMES
        .iter()
        .copied()
        .filter(|name| name.starts_with(&normalized));
    if let (Some(only), None) = (prefixed.next(), prefixed.next()) {
        return Some(only);
    }
    DEPENDENCY_TYPE_NAMES
        .iter()
        .copied()
        .map(|name| (crate::error::levenshtein_distance(&normalized, name), name))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name)
}

fn parse_dependency_type(dep_type: &str) -> Result<DependencyType> {
    let parsed: DependencyType = dep_type.parse().map_err(|_| BeadsError::Validation {
        field: "type".to_string(),
        reason: format!("Invalid dependency type: {dep_type}"),
    })?;

    if let DependencyType::Custom(_) = parsed {
        let hint = nearest_dependency_type(dep_type).map_or_else(
            || {
                format!(
                    "Use --type with one of: {}.",
                    DEPENDENCY_TYPE_NAMES.join(", ")
                )
            },
            |nearest| format!("Did you mean --type {nearest}?"),
        );
        return Err(BeadsError::validation_with_hint(
            "type",
            format!(
                "Unknown dependency type: '{dep_type}'. Allowed types: {}",
                DEPENDENCY_TYPE_NAMES.join(", ")
            ),
            hint,
        ));
    }

    Ok(parsed)
}

fn normalize_dep_type_filter(dep_type: &str) -> Result<String> {
    Ok(parse_dependency_type(dep_type)?.as_str().to_string())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn dep_list(
    args: &DepListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    external_db_paths: &HashMap<String, PathBuf>,
    outer_ctx: &OutputContext,
    quiet: bool,
    no_color: bool,
) -> Result<()> {
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let ctx = OutputContext::from_output_format(output_format, quiet, no_color);
    let issue_id = resolve_issue_id(storage, resolver, &args.issue)?;
    let dep_type_filter = args
        .dep_type
        .as_deref()
        .map(normalize_dep_type_filter)
        .transpose()?;

    let mut items = Vec::new();

    // Get dependencies (what this issue depends on)
    if matches!(args.direction, DepDirection::Down | DepDirection::Both) {
        let deps = storage.get_dependencies_with_metadata(&issue_id)?;
        for dep in deps {
            if let Some(ref filter_type) = dep_type_filter
                && dep.dep_type != *filter_type
            {
                continue;
            }
            items.push(DepListItem {
                issue_id: issue_id.clone(),
                depends_on_id: dep.id.clone(),
                dep_type: dep.dep_type.clone(),
                title: dep.title.clone(),
                status: dep.status.as_str().to_string(),
                priority: dep.priority.0,
            });
        }
    }

    // Get dependents (what depends on this issue)
    if matches!(args.direction, DepDirection::Up | DepDirection::Both) {
        let deps = storage.get_dependents_with_metadata(&issue_id)?;
        for dep in deps {
            if let Some(ref filter_type) = dep_type_filter
                && dep.dep_type != *filter_type
            {
                continue;
            }
            items.push(DepListItem {
                issue_id: dep.id.clone(),
                depends_on_id: issue_id.clone(),
                dep_type: dep.dep_type.clone(),
                title: dep.title.clone(),
                status: dep.status.as_str().to_string(),
                priority: dep.priority.0,
            });
        }
    }

    if !items.is_empty()
        && items.iter().any(|item| {
            item.depends_on_id.starts_with("external:") || item.issue_id.starts_with("external:")
        })
    {
        let external_statuses =
            storage.resolve_external_dependency_statuses(external_db_paths, false)?;
        apply_external_dep_list_metadata(&mut items, &external_statuses);
    }

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    match output_format {
        OutputFormat::Json => {
            ctx.json_pretty(&items);
            return Ok(());
        }
        OutputFormat::Toon => {
            ctx.toon_with_stats(&items, args.stats);
            return Ok(());
        }
        OutputFormat::Text | OutputFormat::Csv => {}
    }

    sort_dep_list_items_for_human(&mut items);

    if items.is_empty() {
        let direction_str = match args.direction {
            DepDirection::Down => "dependencies",
            DepDirection::Up => "dependents",
            DepDirection::Both => "dependencies or dependents",
        };
        ctx.info(&format!(
            "No {direction_str} for {}",
            dep_display_text(&issue_id)
        ));
        return Ok(());
    }

    if ctx.is_rich() {
        // Rich mode: Use panel with tree-like display
        render_dep_list_rich(&ctx, &issue_id, &items, args.direction);
    } else {
        // Plain mode: Simple text output
        let display_issue_id = sanitize_terminal_inline(&issue_id);
        let header = match args.direction {
            DepDirection::Down => {
                format!("Dependencies of {} ({}):", display_issue_id, items.len())
            }
            DepDirection::Up => {
                format!("Dependents of {} ({}):", display_issue_id, items.len())
            }
            DepDirection::Both => format!(
                "Dependencies and dependents of {} ({}):",
                display_issue_id,
                items.len()
            ),
        };
        ctx.info(&header);

        for item in &items {
            let dep_type = sanitize_terminal_inline(&item.dep_type);
            let arrow = if item.issue_id == issue_id {
                format!(
                    "  -> {} ({dep_type})",
                    sanitize_terminal_inline(&item.depends_on_id)
                )
            } else {
                format!(
                    "  <- {} ({dep_type})",
                    sanitize_terminal_inline(&item.issue_id)
                )
            };
            ctx.print_line(&format!(
                "{}: {} [P{}] [{}]",
                arrow,
                sanitize_terminal_inline(&item.title),
                item.priority,
                sanitize_terminal_inline(&item.status)
            ));
        }
    }

    Ok(())
}

/// Render dependency list in rich mode with panel and tree-like display
fn render_dep_list_rich(
    ctx: &OutputContext,
    issue_id: &str,
    items: &[DepListItem],
    direction: DepDirection,
) {
    let theme = ctx.theme();

    // Separate items into dependencies (this issue depends on) and dependents (depend on this)
    let (deps, dependents): (Vec<_>, Vec<_>) =
        items.iter().partition(|item| item.issue_id == issue_id);

    let mut content = Text::new("");

    // Show dependencies (what this issue depends on)
    if !deps.is_empty() && matches!(direction, DepDirection::Down | DepDirection::Both) {
        append_dep_list_section(
            &mut content,
            &dep_list_section_title(true, deps.len()),
            &deps,
            true,
            theme,
        );
    }

    // Add separator if showing both directions
    if !deps.is_empty() && !dependents.is_empty() && matches!(direction, DepDirection::Both) {
        content.append("\n");
    }

    // Show dependents (what depends on this issue)
    if !dependents.is_empty() && matches!(direction, DepDirection::Up | DepDirection::Both) {
        append_dep_list_section(
            &mut content,
            &dep_list_section_title(false, dependents.len()),
            &dependents,
            false,
            theme,
        );
    }

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::new(dep_list_panel_title(direction, issue_id)))
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone());

    ctx.render(&panel);
}

fn dep_list_panel_title(direction: DepDirection, issue_id: &str) -> String {
    let issue_id = sanitize_terminal_inline(issue_id);
    match direction {
        DepDirection::Down => format!("Dependencies for {issue_id}"),
        DepDirection::Up => format!("Dependents for {issue_id}"),
        DepDirection::Both => format!("Dependency relations for {issue_id}"),
    }
}

fn dep_list_section_title(is_dependency_section: bool, count: usize) -> String {
    let label = if is_dependency_section {
        "Dependencies"
    } else {
        "Dependents"
    };
    format!("{label} ({count}):")
}

fn append_dep_list_section(
    content: &mut Text,
    title: &str,
    items: &[&DepListItem],
    use_depends_on_id: bool,
    theme: &Theme,
) {
    content.append_styled(&format!("{title}\n"), theme.emphasis.clone());

    for (i, item) in items.iter().enumerate() {
        let prefix = if i == items.len() - 1 {
            "└── "
        } else {
            "├── "
        };
        let target_id = if use_depends_on_id {
            &item.depends_on_id
        } else {
            &item.issue_id
        };

        content.append_styled(prefix, theme.dimmed.clone());
        content.append_styled(
            sanitize_terminal_inline(target_id).as_ref(),
            theme.issue_id.clone(),
        );
        content.append(" ");
        content.append_styled(
            &format!("({}) ", sanitize_terminal_inline(&item.dep_type)),
            theme.muted.clone(),
        );
        append_dep_list_status(content, &item.status, theme);
        content.append(" ");
        content.append_styled(
            sanitize_terminal_inline(&item.title).as_ref(),
            theme.issue_title.clone(),
        );
        content.append("\n");
    }
}

fn dep_list_status_label(status: &str) -> String {
    match status {
        "open" => "[open]".to_string(),
        "in_progress" => "[in-progress]".to_string(),
        "closed" => "[closed] ✓".to_string(),
        "blocked" => "[blocked]".to_string(),
        _ => sanitize_terminal_inline(status).into_owned(),
    }
}

fn append_dep_list_status(content: &mut Text, status: &str, theme: &Theme) {
    let style = match status {
        "open" => theme.status_open.clone(),
        "in_progress" => theme.status_in_progress.clone(),
        "closed" => theme.status_closed.clone(),
        "blocked" => theme.status_blocked.clone(),
        _ => theme.dimmed.clone(),
    };
    content.append_styled(&dep_list_status_label(status), style);
}

fn apply_external_dep_list_metadata(
    items: &mut [DepListItem],
    external_statuses: &HashMap<String, bool>,
) {
    for item in items {
        let external_id = if item.depends_on_id.starts_with("external:") {
            Some(item.depends_on_id.as_str())
        } else if item.issue_id.starts_with("external:") {
            Some(item.issue_id.as_str())
        } else {
            None
        };

        let Some(external_id) = external_id else {
            continue;
        };

        let satisfied = external_statuses.get(external_id).copied().unwrap_or(false);
        item.status = if satisfied {
            "closed".to_string()
        } else {
            "blocked".to_string()
        };

        let placeholder_title = external_id.strip_prefix("external:").unwrap_or(external_id);
        if item.title.is_empty() || item.title == placeholder_title {
            let prefix = if satisfied { "✓" } else { "⏳" };
            item.title = parse_external_dep_id(external_id).map_or_else(
                || format!("{prefix} {external_id}"),
                |(project, capability)| format!("{prefix} {project}:{capability}"),
            );
        }
    }
}

fn sort_dep_list_items_for_human(items: &mut [DepListItem]) {
    items.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.issue_id.cmp(&right.issue_id))
            .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
            .then_with(|| left.dep_type.cmp(&right.dep_type))
    });
}

fn resolve_dep_tree_node_metadata(
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &crate::model::Issue,
    node_id: &str,
    external_statuses: &HashMap<String, bool>,
) -> Result<(String, i32, String)> {
    if node_id == root_id {
        return Ok((
            root_issue.title.clone(),
            root_issue.priority.0,
            root_issue.status.as_str().to_string(),
        ));
    }

    if node_id.starts_with("external:") {
        let satisfied = external_statuses.get(node_id).copied().unwrap_or(false);
        let status = if satisfied { "closed" } else { "blocked" };
        let prefix = if satisfied { "✓" } else { "⏳" };
        let title = if let Some((project, capability)) = parse_external_dep_id(node_id) {
            format!("{prefix} {project}:{capability}")
        } else {
            format!("{prefix} {node_id}")
        };
        return Ok((title, 2, status.to_string()));
    }

    let issue_opt = storage.get_issue(node_id)?;
    if let Some(issue) = issue_opt {
        return Ok((
            issue.title.clone(),
            issue.priority.0,
            issue.status.as_str().to_string(),
        ));
    }

    // Handle missing/deleted issues gracefully instead of failing the whole tree
    Ok((
        format!("[missing issue: {}]", sanitize_terminal_inline(node_id)),
        2,
        "deleted".to_string(),
    ))
}

fn dep_tree_truncated(depth: usize, max_depth: usize, dependency_count: usize) -> bool {
    depth >= max_depth && dependency_count > 0
}

type DepTreeAdjacency = HashMap<String, Vec<String>>;
type DepTreeMetadataCache = HashMap<String, (String, i32, String)>;

const LOCAL_DEP_TREE_NODE_LIMIT: usize = 256;

fn load_dep_tree_adjacency(
    storage: &SqliteStorage,
) -> Result<(DepTreeAdjacency, DepTreeAdjacency)> {
    let dependency_records = storage.get_all_dependency_records()?;
    let mut dependencies_by_issue: DepTreeAdjacency =
        HashMap::with_capacity(dependency_records.len());
    let mut dependents_by_issue: HashMap<String, Vec<String>> = HashMap::new();

    for (issue_id, dependencies) in dependency_records {
        let dependency_ids = dependencies_by_issue.entry(issue_id.clone()).or_default();
        for dependency in dependencies {
            dependency_ids.push(dependency.depends_on_id.clone());
            dependents_by_issue
                .entry(dependency.depends_on_id)
                .or_default()
                .push(issue_id.clone());
        }
    }

    for dependency_ids in dependencies_by_issue.values_mut() {
        dependency_ids.sort();
        dependency_ids.dedup();
    }
    for dependent_ids in dependents_by_issue.values_mut() {
        dependent_ids.sort();
        dependent_ids.dedup();
    }

    Ok((dependencies_by_issue, dependents_by_issue))
}

fn dep_tree_neighbors(
    direction: DepDirection,
    issue_id: &str,
    dependencies_by_issue: &DepTreeAdjacency,
    dependents_by_issue: &DepTreeAdjacency,
) -> Vec<String> {
    match direction {
        DepDirection::Down => dependencies_by_issue
            .get(issue_id)
            .map_or_else(Vec::new, Clone::clone),
        DepDirection::Up => dependents_by_issue
            .get(issue_id)
            .map_or_else(Vec::new, Clone::clone),
        DepDirection::Both => {
            let mut neighbors = dependencies_by_issue
                .get(issue_id)
                .map_or_else(Vec::new, Clone::clone);
            if let Some(dependents) = dependents_by_issue.get(issue_id) {
                neighbors.extend(dependents.iter().cloned());
            }
            neighbors.sort();
            neighbors.dedup();
            neighbors
        }
    }
}

fn dep_tree_neighbors_from_storage(
    storage: &SqliteStorage,
    direction: DepDirection,
    issue_id: &str,
) -> Result<Vec<String>> {
    let mut neighbors = match direction {
        DepDirection::Down => storage.get_dependencies(issue_id)?,
        DepDirection::Up => storage.get_dependents(issue_id)?,
        DepDirection::Both => {
            let mut neighbors = storage.get_dependencies(issue_id)?;
            neighbors.extend(storage.get_dependents(issue_id)?);
            neighbors
        }
    };
    neighbors.sort();
    neighbors.dedup();
    Ok(neighbors)
}

fn dep_tree_metadata_for_node(
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &crate::model::Issue,
    node_id: &str,
    external_statuses: &HashMap<String, bool>,
    metadata_cache: &mut DepTreeMetadataCache,
) -> Result<(String, i32, String)> {
    if let Some(metadata) = metadata_cache.get(node_id) {
        return Ok(metadata.clone());
    }

    let metadata =
        resolve_dep_tree_node_metadata(storage, root_id, root_issue, node_id, external_statuses)?;
    metadata_cache.insert(node_id.to_string(), metadata.clone());
    Ok(metadata)
}

fn hydrate_dep_tree_metadata_for_ids(
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &crate::model::Issue,
    issue_ids: &[String],
    external_statuses: &HashMap<String, bool>,
    metadata_cache: &mut DepTreeMetadataCache,
) -> Result<()> {
    for issue_id in issue_ids {
        dep_tree_metadata_for_node(
            storage,
            root_id,
            root_issue,
            issue_id,
            external_statuses,
            metadata_cache,
        )?;
    }
    Ok(())
}

struct DepTreeQueueItem {
    id: String,
    depth: usize,
    parent_id: Option<String>,
    parent_key: Option<String>,
    path: Vec<String>,
}

fn dep_tree_root_metadata(root_issue: &crate::model::Issue) -> (String, i32, String) {
    (
        root_issue.title.clone(),
        root_issue.priority.0,
        root_issue.status.as_str().to_string(),
    )
}

#[allow(clippy::too_many_lines)]
fn build_dep_tree_nodes_global(
    args: &DepTreeArgs,
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &crate::model::Issue,
    external_statuses: &HashMap<String, bool>,
) -> Result<Vec<TreeNode>> {
    let mut metadata_cache = storage.get_active_issues_metadata()?;
    metadata_cache.insert(root_id.to_string(), dep_tree_root_metadata(root_issue));
    let (dependencies_by_issue, dependents_by_issue) = load_dep_tree_adjacency(storage)?;

    let mut nodes = Vec::new();
    let _expanded: HashSet<String> = HashSet::new();

    let mut queue = vec![DepTreeQueueItem {
        id: root_id.to_string(),
        depth: 0,
        parent_id: None,
        parent_key: None,
        path: Vec::new(),
    }];
    let mut next_node_key = 0usize;
    // Global expansion guard: a node reachable via multiple (non-ancestor)
    // paths is emitted once under each parent but its subtree is expanded only
    // the first time. Without this, DAGs with shared dependencies (diamonds)
    // enumerate every distinct simple path, which is exponential in depth and
    // exhausts memory (#392).
    let mut expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(item) = queue.pop() {
        // Cycle detection: skip a node that is already one of its own ancestors.
        if item.path.contains(&item.id) {
            continue;
        }

        let node_key = format!("n{next_node_key}");
        next_node_key += 1;

        let (title, priority, status) = dep_tree_metadata_for_node(
            storage,
            root_id,
            root_issue,
            &item.id,
            external_statuses,
            &mut metadata_cache,
        )?;

        let is_external = item.id.starts_with("external:");
        let mut dependencies = Vec::new();
        if !is_external {
            dependencies = dep_tree_neighbors(
                args.direction,
                &item.id,
                &dependencies_by_issue,
                &dependents_by_issue,
            );
        }

        // Expand only if within depth, not external, and not already expanded
        // elsewhere in the graph. A repeat occurrence renders as a shared
        // reference (truncated) rather than re-expanding its subtree.
        let will_expand =
            !is_external && item.depth < args.max_depth && !expanded.contains(&item.id);
        let truncated = if is_external {
            false
        } else if will_expand {
            dep_tree_truncated(item.depth, args.max_depth, dependencies.len())
        } else {
            !dependencies.is_empty()
        };

        // Expand each issue at most once for the whole traversal. A node that
        // is reachable through several parents still gets its own `TreeNode`
        // under every parent, so diamonds stay visible, but only the first
        // occurrence expands the subtree beneath it.
        //
        // Without this the traversal enumerates every distinct simple path
        // from the root, so a graph with shared dependencies produces a node
        // count that grows exponentially with `--max-depth` (GitHub #392: a
        // 121-issue graph emitted 4.19M nodes at depth 40, and a real
        // 1,850-issue graph exhausted 64 GiB of RAM). The correct bound is
        // O(V + E).
        //
        // Claim the "already expanded" slot ONLY when this occurrence really
        // expands. DFS can reach a node at a deeper position first; if a copy
        // that is itself too deep to expand consumed the slot, a later
        // shallower copy would be demoted to a childless repeat and its whole
        // subtree would vanish from the tree.
        let can_expand_here = item.depth < args.max_depth && !item.id.starts_with("external:");
        let already_expanded = expanded.contains(&item.id);
        let expand_now = can_expand_here && !already_expanded;
        if expand_now {
            expanded.insert(item.id.clone());
        }
        let repeat = already_expanded && !dependencies.is_empty();

        nodes.push(TreeNode {
            node_key: node_key.clone(),
            id: item.id.clone(),
            title,
            depth: item.depth,
            parent_id: item.parent_id.clone(),
            parent_key: item.parent_key.clone(),
            priority,
            status,
            truncated,
            repeat,
        });

        // Don't expand if at max depth, or if this subtree is already shown.
        if expand_now {
            let mut new_path = item.path.clone();
            new_path.push(item.id.clone());

            hydrate_dep_tree_metadata_for_ids(
                storage,
                root_id,
                root_issue,
                &dependencies,
                external_statuses,
                &mut metadata_cache,
            )?;
            sort_dep_tree_siblings(&mut dependencies, &metadata_cache);
            // Push in reverse order so first sorted item pops first.
            for dep_id in dependencies.into_iter().rev() {
                queue.push(DepTreeQueueItem {
                    id: dep_id,
                    depth: item.depth + 1,
                    parent_id: Some(item.id.clone()),
                    parent_key: Some(node_key.clone()),
                    path: new_path.clone(),
                });
            }
        }
    }

    Ok(nodes)
}

#[allow(clippy::too_many_lines)]
fn try_build_dep_tree_nodes_local(
    args: &DepTreeArgs,
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &crate::model::Issue,
    external_statuses: &HashMap<String, bool>,
) -> Result<Option<Vec<TreeNode>>> {
    let mut metadata_cache = DepTreeMetadataCache::new();
    metadata_cache.insert(root_id.to_string(), dep_tree_root_metadata(root_issue));

    let mut nodes = Vec::new();
    let _expanded: HashSet<String> = HashSet::new();
    let mut queue = vec![DepTreeQueueItem {
        id: root_id.to_string(),
        depth: 0,
        parent_id: None,
        parent_key: None,
        path: Vec::new(),
    }];
    let mut next_node_key = 0usize;
    // See build_dep_tree_nodes_global: expand each node's subtree once (#392).
    let mut expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(item) = queue.pop() {
        if nodes.len() >= LOCAL_DEP_TREE_NODE_LIMIT {
            return Ok(None);
        }

        if item.path.contains(&item.id) {
            continue;
        }

        let node_key = format!("n{next_node_key}");
        next_node_key += 1;

        let (title, priority, status) = dep_tree_metadata_for_node(
            storage,
            root_id,
            root_issue,
            &item.id,
            external_statuses,
            &mut metadata_cache,
        )?;

        let is_external = item.id.starts_with("external:");
        let mut dependencies = Vec::new();
        if !is_external {
            dependencies = dep_tree_neighbors_from_storage(storage, args.direction, &item.id)?;
        }

        let will_expand =
            !is_external && item.depth < args.max_depth && !expanded.contains(&item.id);
        let truncated = if is_external {
            false
        } else if will_expand {
            dep_tree_truncated(item.depth, args.max_depth, dependencies.len())
        } else {
            !dependencies.is_empty()
        };

        // Mirrors `build_dep_tree_nodes_global`: expand each issue once, but
        // still emit an occurrence under every parent (GitHub #392), and only
        // claim the expansion slot when this occurrence actually expands. The
        // two builders must stay byte-identical — `dep_tree_local_traversal
        // _matches_global_nodes` compares their projections including
        // `node_key`.
        let can_expand_here = item.depth < args.max_depth && !item.id.starts_with("external:");
        let already_expanded = expanded.contains(&item.id);
        let expand_now = can_expand_here && !already_expanded;
        if expand_now {
            expanded.insert(item.id.clone());
        }
        let repeat = already_expanded && !dependencies.is_empty();

        nodes.push(TreeNode {
            node_key: node_key.clone(),
            id: item.id.clone(),
            title,
            depth: item.depth,
            parent_id: item.parent_id.clone(),
            parent_key: item.parent_key.clone(),
            priority,
            status,
            truncated,
            repeat,
        });

        if expand_now {
            if nodes.len().saturating_add(dependencies.len()) > LOCAL_DEP_TREE_NODE_LIMIT {
                return Ok(None);
            }

            expanded.insert(item.id.clone());
            let mut new_path = item.path.clone();
            new_path.push(item.id.clone());

            hydrate_dep_tree_metadata_for_ids(
                storage,
                root_id,
                root_issue,
                &dependencies,
                external_statuses,
                &mut metadata_cache,
            )?;
            sort_dep_tree_siblings(&mut dependencies, &metadata_cache);

            for dep_id in dependencies.into_iter().rev() {
                queue.push(DepTreeQueueItem {
                    id: dep_id,
                    depth: item.depth + 1,
                    parent_id: Some(item.id.clone()),
                    parent_key: Some(node_key.clone()),
                    path: new_path.clone(),
                });
            }
        }
    }

    Ok(Some(nodes))
}

#[allow(clippy::too_many_lines)]
fn dep_tree(
    args: &DepTreeArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    external_db_paths: &HashMap<String, PathBuf>,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let root_id = resolve_issue_id(storage, resolver, &args.issue)?;
    let root_issue = storage
        .get_issue(&root_id)?
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: root_id.clone(),
        })?;

    let external_statuses =
        storage.resolve_external_dependency_statuses(external_db_paths, false)?;
    let nodes = match try_build_dep_tree_nodes_local(
        args,
        storage,
        &root_id,
        &root_issue,
        &external_statuses,
    )? {
        Some(nodes) => nodes,
        None => {
            build_dep_tree_nodes_global(args, storage, &root_id, &root_issue, &external_statuses)?
        }
    };

    if ctx.is_json() || ctx.is_toon() {
        if ctx.is_toon() {
            ctx.toon(&nodes);
        } else {
            ctx.json_pretty(&nodes);
        }
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    // Mermaid format output
    if args.format.eq_ignore_ascii_case("mermaid") {
        render_dep_tree_mermaid(&nodes);
        return Ok(());
    }

    // Text tree output
    if nodes.is_empty() {
        ctx.info(&format!("No dependency tree for {root_id}"));
        return Ok(());
    }

    if ctx.is_rich() {
        // Rich mode: Use tree component with styled output
        render_dep_tree_rich(ctx, &nodes);
    } else {
        // Plain mode: Simple indented text
        for node in &nodes {
            let indent = "  ".repeat(node.depth);
            let prefix = if node.depth == 0 {
                ""
            } else if node.truncated {
                "├── (truncated) "
            } else if node.repeat {
                "├── (shown above) "
            } else {
                "├── "
            };
            ctx.print_line(&format!(
                "{}{}{}: {} [P{}] [{}]",
                indent,
                prefix,
                sanitize_terminal_inline(&node.id),
                sanitize_terminal_inline(&node.title),
                node.priority,
                sanitize_terminal_inline(&node.status)
            ));
        }
    }

    Ok(())
}

fn sanitize_mermaid_label(text: &str) -> String {
    sanitize_terminal_inline(text)
        .replace('"', "'")
        .replace(['\n', '\r'], " ")
}

fn render_dep_tree_mermaid(nodes: &[TreeNode]) {
    // Use println! directly to avoid rich_rust markup interpretation
    println!("graph TD");

    for node in nodes {
        let escaped_id = sanitize_mermaid_label(&node.id);
        let escaped_title = sanitize_mermaid_label(&node.title);
        println!(
            "    {}[\"{}: {} [P{}]\"]",
            node.node_key, escaped_id, escaped_title, node.priority
        );
    }

    for node in nodes {
        if let Some(parent_key) = node.parent_key.as_deref() {
            println!("    {parent_key} --> {}", node.node_key);
        }
    }
}

fn sort_dep_tree_siblings(
    dependencies: &mut [String],
    metadata_cache: &HashMap<String, (String, i32, String)>,
) {
    dependencies.sort_by(|left, right| {
        let left_meta = metadata_cache.get(left);
        let right_meta = metadata_cache.get(right);

        dep_tree_sibling_priority(left_meta)
            .cmp(&dep_tree_sibling_priority(right_meta))
            .then_with(|| {
                dep_tree_sibling_status_rank(left_meta)
                    .cmp(&dep_tree_sibling_status_rank(right_meta))
            })
            .then_with(|| dep_tree_sibling_title(left_meta).cmp(dep_tree_sibling_title(right_meta)))
            .then_with(|| left.cmp(right))
    });
}

fn dep_tree_sibling_priority(meta: Option<&(String, i32, String)>) -> i32 {
    meta.map_or(i32::MAX, |(_, priority, _)| *priority)
}

fn dep_tree_sibling_title(meta: Option<&(String, i32, String)>) -> &str {
    meta.map_or("", |(title, _, _)| title.as_str())
}

fn dep_tree_sibling_status_rank(meta: Option<&(String, i32, String)>) -> u8 {
    let Some((_, _, status)) = meta else {
        return u8::MAX;
    };

    match status.as_str() {
        "open" => 0,
        "in_progress" => 1,
        "blocked" => 2,
        "deferred" => 3,
        "closed" => 4,
        "deleted" | "tombstone" => 5,
        _ => 6,
    }
}

/// Render dependency tree in rich mode using Tree component
fn render_dep_tree_rich(ctx: &OutputContext, nodes: &[TreeNode]) {
    if nodes.is_empty() {
        return;
    }

    let theme = ctx.theme();

    // Group nodes by parent_key for O(1) lookups
    let mut children_map: std::collections::HashMap<Option<&str>, Vec<&TreeNode>> =
        std::collections::HashMap::new();
    for node in nodes {
        children_map
            .entry(node.parent_key.as_deref())
            .or_default()
            .push(node);
    }

    // Build tree structure from flat nodes list
    let root = build_tree_node_rich(&nodes[0], &children_map, theme);
    let tree = Tree::new(root)
        .guides(TreeGuides::Rounded)
        .guide_style(theme.dimmed.clone());

    ctx.render(&tree);
}

/// Recursively build a tree node for rich rendering
fn build_tree_node_rich<'a>(
    node: &'a TreeNode,
    children_map: &std::collections::HashMap<Option<&'a str>, Vec<&'a TreeNode>>,
    theme: &Theme,
) -> rich_rust::renderables::TreeNode {
    let mut tree_node = rich_rust::renderables::TreeNode::new(build_tree_node_label(node, theme));

    // Find and add children using the pre-computed map
    if let Some(children) = children_map.get(&Some(node.node_key.as_str())) {
        for child in children {
            let child_node = build_tree_node_rich(child, children_map, theme);
            tree_node = tree_node.child(child_node);
        }
    }

    tree_node
}

fn build_tree_node_label(node: &TreeNode, theme: &Theme) -> Text {
    let mut label = Text::new("");
    label.append_styled(
        sanitize_terminal_inline(&node.id).as_ref(),
        theme.issue_id.clone(),
    );
    label.append(" [");
    label.append_styled(
        sanitize_terminal_inline(&node.status).as_ref(),
        dep_tree_status_style(&node.status, theme),
    );
    label.append("]");
    if let Some(indicator) = dep_tree_status_indicator(&node.status) {
        label.append_styled(indicator, dep_tree_status_style(&node.status, theme));
    }
    label.append(" ");
    label.append_styled(
        &truncate_title(&node.title, if node.truncated { 35 } else { 40 }),
        theme.issue_title.clone(),
    );
    if node.truncated {
        label.append_styled(" (truncated)", theme.dimmed.clone());
    }
    if node.repeat {
        label.append_styled(" (shown above)", theme.dimmed.clone());
    }
    label
}

fn dep_tree_status_style(status: &str, theme: &Theme) -> Style {
    match status {
        "open" => theme.status_open.clone(),
        "in_progress" => theme.status_in_progress.clone(),
        "closed" | "deleted" | "tombstone" => theme.status_closed.clone(),
        "blocked" => theme.status_blocked.clone(),
        "deferred" => theme.status_deferred.clone(),
        _ => theme.muted.clone(),
    }
}

fn dep_tree_status_indicator(status: &str) -> Option<&'static str> {
    match status {
        "closed" => Some(" ✓"),
        "blocked" => Some(" ⚠"),
        _ => None,
    }
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

fn dep_cycles(
    args: &DepCyclesArgs,
    storage: &SqliteStorage,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let report = storage.detect_dependency_cycle_report(args.blocking_only)?;
    let active_count = report.active_cycles.len();
    let archived_closed_count = report.archived_closed_cycles.len();
    let total_count = active_count + archived_closed_count;

    // #368: An active dependency cycle is a machine-actionable condition, so a
    // scripted/robot caller gating on the exit code must be able to see it. We
    // still emit the full, data-carrying output on every surface below (text,
    // rich, JSON `count`, TOON) — the exit code is recorded here and applied by
    // `main` after output completes, so the JSON/TOON stream stays a single
    // clean object. Archived-closed-only cycles are historical and never flip
    // the exit code, even under `--include-closed`.
    if active_count > 0 {
        crate::output::record_pending_exit_code(crate::error::ErrorCode::CycleDetected.exit_code());
    }
    let mut cycles = report.active_cycles.clone();
    let mut active_cycles = Vec::new();
    let mut archived_closed_cycles = Vec::new();

    if args.include_closed {
        active_cycles.clone_from(&report.active_cycles);
        archived_closed_cycles.clone_from(&report.archived_closed_cycles);
        cycles.extend(report.archived_closed_cycles.clone());
        cycles.sort();
    }
    let count = cycles.len();
    let scope = if args.include_closed {
        "active_and_archived"
    } else {
        "active"
    };

    if ctx.is_json() || ctx.is_toon() {
        let result = CyclesResult {
            cycles,
            count,
            active_count,
            archived_closed_count,
            total_count,
            blocking_only: args.blocking_only,
            include_closed: args.include_closed,
            scope,
            active_cycles,
            archived_closed_cycles,
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    let cycle_scope = cycle_scope_label(args.blocking_only);
    if count == 0 {
        if archived_closed_count > 0 && !args.include_closed {
            ctx.success(&format!(
                "No active {cycle_scope} cycles detected. {archived_closed_count} archived closed-only cycle(s) hidden; rerun with --include-closed to inspect them."
            ));
        } else {
            ctx.success(&format!("No {cycle_scope} cycles detected."));
        }
    } else if ctx.is_rich() {
        // Rich mode: Show cycles with red highlighting in a panel
        render_cycles_rich(ctx, &cycles, count, args.blocking_only);
    } else {
        // Plain mode: Simple text output
        ctx.warning(&format!("Found {count} {cycle_scope} cycle(s):"));
        for (i, cycle) in cycles.iter().enumerate() {
            ctx.print_line(&format!("  {}. {}", i + 1, format_cycle_plain(cycle)));
        }
    }

    Ok(())
}

/// Render cycles in rich mode with red highlighting
fn render_cycles_rich(
    ctx: &OutputContext,
    cycles: &[Vec<String>],
    count: usize,
    blocking_only: bool,
) {
    let theme = ctx.theme();
    let content = build_cycles_rich_text(cycles, count, theme, blocking_only);
    let title = if blocking_only {
        "Blocking Dependency Cycles"
    } else {
        "Dependency Cycles"
    };
    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::new(title))
        .border_style(theme.error.clone());

    ctx.render(&panel);
}

fn cycle_scope_label(blocking_only: bool) -> &'static str {
    if blocking_only {
        "blocking dependency"
    } else {
        "dependency"
    }
}

fn build_cycles_rich_text(
    cycles: &[Vec<String>],
    count: usize,
    theme: &Theme,
    blocking_only: bool,
) -> Text {
    let mut content = Text::new("");
    let cycle_scope = cycle_scope_label(blocking_only);
    content.append_styled(
        &format!("⚠ {count} {cycle_scope} cycle(s) detected:\n\n"),
        theme.error.clone().bold(),
    );

    for (i, cycle) in cycles.iter().enumerate() {
        content.append_styled(&format!("Cycle {}:\n", i + 1), theme.emphasis.clone());
        content.append("  ");
        append_cycle_path_rich(&mut content, cycle, theme);
        content.append("\n");

        // Add underline visual
        let path_len = format_cycle_plain(cycle).chars().count();
        content.append_styled(
            &format!("  {}\n", "^".repeat(path_len.min(60))),
            theme.error.clone(),
        );

        if i < cycles.len() - 1 {
            content.append("\n");
        }
    }

    content.append("\n");
    content.append_styled(
        "Suggestion: Remove one dependency from each cycle to break it.",
        theme.dimmed.clone(),
    );

    content
}

fn append_cycle_path_rich(content: &mut Text, cycle: &[String], theme: &Theme) {
    for (index, id) in cycle.iter().enumerate() {
        if index > 0 {
            content.append_styled(" → ", theme.error.clone());
        }
        content.append_styled(sanitize_terminal_inline(id).as_ref(), theme.error.clone());
    }
}

fn format_cycle_plain(cycle: &[String]) -> String {
    cycle
        .iter()
        .map(|id| sanitize_terminal_inline(id).into_owned())
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::init_test_logging;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use tracing::info;

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
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

    fn test_dep_list_item(issue_id: &str, depends_on_id: &str, priority: i32) -> DepListItem {
        DepListItem {
            issue_id: issue_id.to_string(),
            depends_on_id: depends_on_id.to_string(),
            dep_type: "blocks".to_string(),
            title: depends_on_id.to_string(),
            status: "open".to_string(),
            priority,
        }
    }

    #[test]
    fn test_dependency_type_parsing() {
        init_test_logging();
        info!("test_dependency_type_parsing: starting");
        assert_eq!(
            "blocks".parse::<DependencyType>().unwrap(),
            DependencyType::Blocks
        );
        assert_eq!(
            "parent-child".parse::<DependencyType>().unwrap(),
            DependencyType::ParentChild
        );
        assert_eq!(
            "related".parse::<DependencyType>().unwrap(),
            DependencyType::Related
        );
        assert_eq!(
            "duplicates".parse::<DependencyType>().unwrap(),
            DependencyType::Duplicates
        );
        info!("test_dependency_type_parsing: assertions passed");
    }

    #[test]
    fn test_blocking_dependency_types() {
        init_test_logging();
        info!("test_blocking_dependency_types: starting");
        assert!(DependencyType::Blocks.is_blocking());
        assert!(DependencyType::ParentChild.is_blocking());
        assert!(!DependencyType::Related.is_blocking());
        assert!(!DependencyType::Duplicates.is_blocking());
        info!("test_blocking_dependency_types: assertions passed");
    }

    #[test]
    fn test_normalize_dep_type_filter_canonicalizes_standard_types() {
        assert_eq!(
            normalize_dep_type_filter("Parent-Child").unwrap(),
            "parent-child"
        );
        assert_eq!(normalize_dep_type_filter("BLOCKS").unwrap(), "blocks");
    }

    #[test]
    fn test_normalize_dep_type_filter_rejects_unknown_types() {
        // Unknown spellings are refused with the actionable hint variant
        // (docs-shaped mistakes name the nearest real type).
        let err = normalize_dep_type_filter("parent_child").unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::ValidationWithHint { field, hint, .. }
                    if field == "type" && hint.contains("parent-child")
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_parse_dependency_import_line_accepts_edge_jsonl() {
        let deps = parse_dependency_import_line(
            r#"{"issue_id":"bd-a","depends_on_id":"bd-b","type":"parent-child"}"#,
            7,
        )
        .unwrap();

        assert_eq!(
            deps,
            vec![BulkDependencyInsert {
                issue_id: "bd-a".to_string(),
                depends_on_id: "bd-b".to_string(),
                dep_type: "parent-child".to_string(),
            }]
        );
    }

    #[test]
    fn test_parse_dependency_import_line_accepts_issue_jsonl_dependencies() {
        let deps = parse_dependency_import_line(
            r#"{"id":"bd-a","dependencies":[{"depends_on_id":"bd-b","type":"blocks"},{"issue_id":"bd-c","depends_on_id":"bd-d","dep_type":"waits-for"}]}"#,
            11,
        )
        .unwrap();

        assert_eq!(
            deps,
            vec![
                BulkDependencyInsert {
                    issue_id: "bd-a".to_string(),
                    depends_on_id: "bd-b".to_string(),
                    dep_type: "blocks".to_string(),
                },
                BulkDependencyInsert {
                    issue_id: "bd-c".to_string(),
                    depends_on_id: "bd-d".to_string(),
                    dep_type: "waits-for".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parse_dependency_import_line_skips_issue_record_without_dependencies() {
        let deps = parse_dependency_import_line(
            r#"{"id":"bd-no-deps","title":"plain issue record","status":"open"}"#,
            13,
        )
        .unwrap();

        assert!(deps.is_empty());
    }

    #[test]
    fn test_parse_dependency_import_line_rejects_dependency_array_without_owner() {
        let error = parse_dependency_import_line(
            r#"{"dependencies":[{"depends_on_id":"bd-target","type":"blocks"}]}"#,
            17,
        )
        .unwrap_err();

        assert!(
            matches!(&error, BeadsError::Validation { field, .. } if field == "issue_id"),
            "unexpected missing-owner error: {error:?}"
        );
    }

    #[test]
    fn test_parse_dependency_import_line_rejects_empty_edge_ids() {
        let error = parse_dependency_import_line(
            r#"{"issue_id":"  ","depends_on_id":"bd-target","type":"blocks"}"#,
            19,
        )
        .unwrap_err();

        assert!(
            matches!(&error, BeadsError::Validation { field, .. } if field == "jsonl"),
            "unexpected empty-id error: {error:?}"
        );
    }

    #[test]
    fn test_dep_import_bulk_storage_path_inserts_parent_child_batch() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for id in ["bd-parent", "bd-child-a", "bd-child-b"] {
            storage
                .create_issue(&make_test_issue(id, id), "tester")
                .unwrap();
        }

        let inserted = storage
            .add_dependencies_bulk_for_import(
                &[
                    BulkDependencyInsert {
                        issue_id: "bd-child-a".to_string(),
                        depends_on_id: "bd-parent".to_string(),
                        dep_type: "parent-child".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-child-b".to_string(),
                        depends_on_id: "bd-parent".to_string(),
                        dep_type: "parent-child".to_string(),
                    },
                ],
                "tester",
            )
            .unwrap();

        assert_eq!(inserted, 2);
        assert_eq!(
            storage.get_dependencies("bd-child-a").unwrap(),
            vec!["bd-parent".to_string()]
        );
        assert_eq!(
            storage.get_dependencies("bd-child-b").unwrap(),
            vec!["bd-parent".to_string()]
        );
    }

    #[test]
    fn test_dep_import_bulk_storage_path_skips_type_distinct_duplicate_pairs() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for id in ["bd-source", "bd-target"] {
            storage
                .create_issue(&make_test_issue(id, id), "tester")
                .unwrap();
        }

        let inserted = storage
            .add_dependencies_bulk_for_import(
                &[
                    BulkDependencyInsert {
                        issue_id: "bd-source".to_string(),
                        depends_on_id: "bd-target".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-source".to_string(),
                        depends_on_id: "bd-target".to_string(),
                        dep_type: "related".to_string(),
                    },
                ],
                "tester",
            )
            .unwrap();

        assert_eq!(inserted, 1);
        let dep_types: Vec<String> = storage
            .get_dependencies_full("bd-source")
            .unwrap()
            .into_iter()
            .map(|dep| dep.dep_type.as_str().to_string())
            .collect();
        assert_eq!(dep_types, vec!["blocks".to_string()]);
    }

    #[test]
    fn test_add_dependency() {
        init_test_logging();
        info!("test_add_dependency: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        // Add dependency: bd-001 depends on bd-002 (blocks)
        let added = storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        assert!(added);

        // Adding same dependency again should return false
        let added_again = storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        assert!(!added_again);
        info!("test_add_dependency: assertions passed");
    }

    #[test]
    fn test_remove_dependency() {
        init_test_logging();
        info!("test_remove_dependency: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();

        let removed = storage
            .remove_dependency("bd-001", "bd-002", "tester")
            .unwrap();
        assert!(removed);

        // Removing again should return false
        let removed_again = storage
            .remove_dependency("bd-001", "bd-002", "tester")
            .unwrap();
        assert!(!removed_again);
        info!("test_remove_dependency: assertions passed");
    }

    #[test]
    fn test_get_dependencies() {
        init_test_logging();
        info!("test_get_dependencies: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        let issue3 = make_test_issue("bd-003", "Issue 3");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // bd-001 depends on bd-002 and bd-003
        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-001", "bd-003", "blocks", "tester")
            .unwrap();

        let deps = storage.get_dependencies("bd-001").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"bd-002".to_string()));
        assert!(deps.contains(&"bd-003".to_string()));
        info!("test_get_dependencies: assertions passed");
    }

    #[test]
    fn test_get_dependents() {
        init_test_logging();
        info!("test_get_dependents: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        let issue3 = make_test_issue("bd-003", "Issue 3");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // bd-002 and bd-003 depend on bd-001
        storage
            .add_dependency("bd-002", "bd-001", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-003", "bd-001", "blocks", "tester")
            .unwrap();

        let dependents = storage.get_dependents("bd-001").unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&"bd-002".to_string()));
        assert!(dependents.contains(&"bd-003".to_string()));
        info!("test_get_dependents: assertions passed");
    }

    #[test]
    fn test_dep_tree_adjacency_prefetch_matches_direct_queries() {
        init_test_logging();
        info!("test_dep_tree_adjacency_prefetch_matches_direct_queries: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        for issue in [
            make_test_issue("bd-001", "Issue 1"),
            make_test_issue("bd-002", "Issue 2"),
            make_test_issue("bd-003", "Issue 3"),
            make_test_issue("bd-004", "Issue 4"),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-001", "bd-003", "related", "tester")
            .unwrap();
        storage
            .add_dependency("bd-004", "bd-001", "blocks", "tester")
            .unwrap();

        let (dependencies_by_issue, dependents_by_issue) =
            load_dep_tree_adjacency(&storage).unwrap();

        let mut direct_down = storage.get_dependencies("bd-001").unwrap();
        direct_down.sort();
        let down = dep_tree_neighbors(
            DepDirection::Down,
            "bd-001",
            &dependencies_by_issue,
            &dependents_by_issue,
        );
        assert_eq!(down, direct_down);

        let mut direct_up = storage.get_dependents("bd-001").unwrap();
        direct_up.sort();
        let up = dep_tree_neighbors(
            DepDirection::Up,
            "bd-001",
            &dependencies_by_issue,
            &dependents_by_issue,
        );
        assert_eq!(up, direct_up);

        let both = dep_tree_neighbors(
            DepDirection::Both,
            "bd-001",
            &dependencies_by_issue,
            &dependents_by_issue,
        );
        assert_eq!(
            both,
            vec![
                "bd-002".to_string(),
                "bd-003".to_string(),
                "bd-004".to_string(),
            ]
        );
        info!("test_dep_tree_adjacency_prefetch_matches_direct_queries: assertions passed");
    }

    fn dep_tree_test_args(issue: &str, direction: DepDirection, max_depth: usize) -> DepTreeArgs {
        DepTreeArgs {
            issue: issue.to_string(),
            direction,
            max_depth,
            format: "text".to_string(),
        }
    }

    type TreeNodeProjection = (
        String,
        String,
        String,
        usize,
        Option<String>,
        Option<String>,
        i32,
        String,
        bool,
        bool,
    );

    fn tree_node_projection(nodes: &[TreeNode]) -> Vec<TreeNodeProjection> {
        nodes
            .iter()
            .map(|node| {
                (
                    node.node_key.clone(),
                    node.id.clone(),
                    node.title.clone(),
                    node.depth,
                    node.parent_id.clone(),
                    node.parent_key.clone(),
                    node.priority,
                    node.status.clone(),
                    node.truncated,
                    node.repeat,
                )
            })
            .collect()
    }

    #[test]
    fn test_dep_tree_local_traversal_matches_global_nodes() {
        init_test_logging();
        info!("test_dep_tree_local_traversal_matches_global_nodes: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        for issue in [
            make_test_issue("bd-001", "Issue 1"),
            make_test_issue("bd-002", "Issue 2"),
            make_test_issue("bd-003", "Issue 3"),
            make_test_issue("bd-004", "Issue 4"),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }
        let mut low_priority = make_test_issue("bd-005", "Issue 5");
        low_priority.priority = Priority(3);
        storage.create_issue(&low_priority, "tester").unwrap();

        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-001", "bd-003", "related", "tester")
            .unwrap();
        storage
            .add_dependency("bd-001", "bd-005", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-001", "external:ext:cap", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-004", "bd-001", "blocks", "tester")
            .unwrap();

        let args = dep_tree_test_args("bd-001", DepDirection::Both, 2);
        let root_issue = storage.get_issue("bd-001").unwrap().unwrap();
        let external_statuses = HashMap::new();
        let local = try_build_dep_tree_nodes_local(
            &args,
            &storage,
            "bd-001",
            &root_issue,
            &external_statuses,
        )
        .unwrap()
        .expect("small tree should use local traversal");
        let global =
            build_dep_tree_nodes_global(&args, &storage, "bd-001", &root_issue, &external_statuses)
                .unwrap();

        assert_eq!(tree_node_projection(&local), tree_node_projection(&global));
        info!("test_dep_tree_local_traversal_matches_global_nodes: assertions passed");
    }

    /// GitHub #392: a "diamond ladder" (`A_i` depends on `B_i` and `C_i`; both
    /// depend on `A_{i+1}`) used to emit one node per distinct simple path, so
    /// the node count doubled with every rung and grew without bound as
    /// `--max-depth` rose. The output is now bounded by the graph size no
    /// matter how deep the traversal is allowed to go.
    #[test]
    fn test_dep_tree_diamond_ladder_is_bounded_by_graph_size() {
        const RUNGS: usize = 12;

        init_test_logging();
        info!("test_dep_tree_diamond_ladder_is_bounded_by_graph_size: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let a_id = |i: usize| format!("bd-a{i:03}");
        let b_id = |i: usize| format!("bd-b{i:03}");
        let c_id = |i: usize| format!("bd-c{i:03}");

        for index in 0..=RUNGS {
            let issue = make_test_issue(&a_id(index), &format!("A{index}"));
            storage.create_issue(&issue, "tester").unwrap();
        }
        for index in 0..RUNGS {
            for id in [b_id(index), c_id(index)] {
                let issue = make_test_issue(&id, &format!("Rung {index}"));
                storage.create_issue(&issue, "tester").unwrap();
            }
        }
        for index in 0..RUNGS {
            for id in [b_id(index), c_id(index)] {
                storage
                    .add_dependency(&a_id(index), &id, "blocks", "tester")
                    .unwrap();
                storage
                    .add_dependency(&id, &a_id(index + 1), "blocks", "tester")
                    .unwrap();
            }
        }

        let total_issues = 3 * RUNGS + 1;
        // Each rung contributes A->B, A->C, B->A', C->A'.
        let total_edges = 4 * RUNGS;
        let root_issue = storage.get_issue(&a_id(0)).unwrap().unwrap();
        let external_statuses = HashMap::new();

        let node_count_at = |max_depth: usize| {
            let args = dep_tree_test_args(&a_id(0), DepDirection::Down, max_depth);
            build_dep_tree_nodes_global(&args, &storage, &a_id(0), &root_issue, &external_statuses)
                .unwrap()
        };

        // Depth far beyond the ladder length: the pre-fix traversal emitted
        // 2^RUNGS-scale node counts here.
        let global = node_count_at(500);

        // Every issue is expanded once, so the emitted occurrences are the root
        // plus one per edge walked out of an expanded node — O(V + E), not one
        // node per distinct simple path.
        assert_eq!(
            global.len(),
            total_edges + 1,
            "expected O(V+E) nodes for a {total_issues}-issue / {total_edges}-edge graph"
        );

        // The real regression signature: node count must not grow with depth
        // once the traversal has covered the graph.
        assert_eq!(
            node_count_at(40).len(),
            global.len(),
            "node count must not grow with --max-depth"
        );

        // Every issue in the ladder is still reachable in the rendered tree.
        let rendered: HashSet<&str> = global.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(rendered.len(), total_issues);

        // The shared `A_{i+1}` rungs are reached through both B and C, so
        // exactly one occurrence of each expands and the traversal is finite.
        let repeats = global.iter().filter(|node| node.repeat).count();
        assert!(
            repeats > 0,
            "diamond joins should be marked as repeat occurrences"
        );

        info!("test_dep_tree_diamond_ladder_is_bounded_by_graph_size: assertions passed");
    }

    /// A node reachable at two different depths must still expand at the
    /// shallow one even if the deep occurrence was visited first.
    ///
    /// DFS reaches the deeper copy first here. If the traversal claimed the
    /// "already expanded" slot for an occurrence that was itself too deep to
    /// expand, the shallow copy would render as a childless repeat and the
    /// subtree under it would disappear from the tree entirely.
    #[test]
    fn test_dep_tree_expands_shallow_occurrence_reached_after_deep_one() {
        init_test_logging();
        info!("test_dep_tree_expands_shallow_occurrence_reached_after_deep_one: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        for (id, title) in [
            ("bd-root", "Root"),
            ("bd-a-mid", "A mid"),
            ("bd-x-shared", "X shared"),
            ("bd-y-leaf", "Y leaf"),
        ] {
            let issue = make_test_issue(id, title);
            storage.create_issue(&issue, "tester").unwrap();
        }

        // Root -> A (sorts first) and Root -> X directly; A -> X makes X
        // reachable at depth 1 and depth 2. X -> Y is the subtree at risk.
        storage
            .add_dependency("bd-root", "bd-a-mid", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-root", "bd-x-shared", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-a-mid", "bd-x-shared", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-x-shared", "bd-y-leaf", "blocks", "tester")
            .unwrap();

        // Y sits at depth 2 via the shallow X, exactly at the depth limit.
        let args = dep_tree_test_args("bd-root", DepDirection::Down, 2);
        let root_issue = storage.get_issue("bd-root").unwrap().unwrap();
        let external_statuses = HashMap::new();

        let global = build_dep_tree_nodes_global(
            &args,
            &storage,
            "bd-root",
            &root_issue,
            &external_statuses,
        )
        .unwrap();
        assert!(
            global.iter().any(|node| node.id == "bd-y-leaf"),
            "the subtree under the shallow occurrence must still be rendered: {:?}",
            global.iter().map(|n| (&n.id, n.depth)).collect::<Vec<_>>()
        );

        let local = try_build_dep_tree_nodes_local(
            &args,
            &storage,
            "bd-root",
            &root_issue,
            &external_statuses,
        )
        .unwrap()
        .expect("small tree should use local traversal");
        assert_eq!(tree_node_projection(&local), tree_node_projection(&global));

        info!("test_dep_tree_expands_shallow_occurrence_reached_after_deep_one: assertions passed");
    }

    #[test]
    fn test_dep_tree_local_traversal_falls_back_for_wide_roots() {
        init_test_logging();
        info!("test_dep_tree_local_traversal_falls_back_for_wide_roots: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let root = make_test_issue("bd-root", "Root");
        storage.create_issue(&root, "tester").unwrap();
        for index in 0..LOCAL_DEP_TREE_NODE_LIMIT {
            let child_id = format!("bd-child-{index:03}");
            let child = make_test_issue(&child_id, &format!("Child {index:03}"));
            storage.create_issue(&child, "tester").unwrap();
            storage
                .add_dependency("bd-root", &child_id, "blocks", "tester")
                .unwrap();
        }

        let args = dep_tree_test_args("bd-root", DepDirection::Down, 10);
        let root_issue = storage.get_issue("bd-root").unwrap().unwrap();
        let external_statuses = HashMap::new();
        let local = try_build_dep_tree_nodes_local(
            &args,
            &storage,
            "bd-root",
            &root_issue,
            &external_statuses,
        )
        .unwrap();

        assert!(local.is_none());
        info!("test_dep_tree_local_traversal_falls_back_for_wide_roots: assertions passed");
    }

    #[test]
    fn test_dep_tree_diamond_graph_is_bounded() {
        // Regression for #392: a "diamond ladder" DAG (A_i depends on B_i and
        // C_i; both depend on A_{i+1}) is reachable via 2^i distinct simple
        // paths. Without a global expansion guard the traversal emitted
        // 2^(depth/2+2)-3 nodes and exhausted memory. With expansion-once the
        // node count is bounded by the number of edges regardless of depth.
        const RUNGS: usize = 20;

        init_test_logging();
        info!("test_dep_tree_diamond_graph_is_bounded: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let a: Vec<String> = (0..=RUNGS).map(|i| format!("bd-a{i:03}")).collect();
        let b: Vec<String> = (0..RUNGS).map(|i| format!("bd-b{i:03}")).collect();
        let c: Vec<String> = (0..RUNGS).map(|i| format!("bd-c{i:03}")).collect();

        for id in a.iter().chain(b.iter()).chain(c.iter()) {
            storage
                .create_issue(&make_test_issue(id, id), "tester")
                .unwrap();
        }
        for i in 0..RUNGS {
            storage
                .add_dependency(&a[i], &b[i], "blocks", "tester")
                .unwrap();
            storage
                .add_dependency(&a[i], &c[i], "blocks", "tester")
                .unwrap();
            storage
                .add_dependency(&b[i], &a[i + 1], "blocks", "tester")
                .unwrap();
            storage
                .add_dependency(&c[i], &a[i + 1], "blocks", "tester")
                .unwrap();
        }

        let total_issues = a.len() + b.len() + c.len();
        let total_edges = 4 * RUNGS;
        // Deep traversal: the per-path traversal would need 2^(100/2+2) nodes.
        let args = dep_tree_test_args(&a[0], DepDirection::Down, 100);
        let root_issue = storage.get_issue(&a[0]).unwrap().unwrap();
        let external_statuses = HashMap::new();

        let global =
            build_dep_tree_nodes_global(&args, &storage, &a[0], &root_issue, &external_statuses)
                .unwrap();

        // Node count is bounded by root + one occurrence per edge (each node is
        // rendered under every parent, but its subtree is expanded only once).
        assert!(
            global.len() <= total_edges + 1,
            "expected <= {} nodes, got {}",
            total_edges + 1,
            global.len()
        );
        // Every unique issue in the graph is present at least once.
        let seen: std::collections::HashSet<&str> = global.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            seen.len(),
            total_issues,
            "every unique issue should be represented"
        );

        // The local fast path must not blow up either: it either produces the
        // same bounded set or cleanly falls back to the global path.
        if let Some(local) =
            try_build_dep_tree_nodes_local(&args, &storage, &a[0], &root_issue, &external_statuses)
                .unwrap()
        {
            assert_eq!(tree_node_projection(&local), tree_node_projection(&global));
        }
        info!("test_dep_tree_diamond_graph_is_bounded: assertions passed");
    }

    #[test]
    fn test_cycle_detection_simple() {
        init_test_logging();
        info!("test_cycle_detection_simple: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        // bd-001 depends on bd-002
        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();

        // bd-002 depends on bd-001 would create a cycle
        let would_cycle = storage
            .would_create_cycle("bd-002", "bd-001", true)
            .unwrap();
        assert!(would_cycle);
        info!("test_cycle_detection_simple: assertions passed");
    }

    #[test]
    fn test_cycle_detection_transitive() {
        init_test_logging();
        info!("test_cycle_detection_transitive: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        let issue3 = make_test_issue("bd-003", "Issue 3");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // bd-001 -> bd-002 -> bd-003
        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-002", "bd-003", "blocks", "tester")
            .unwrap();

        // bd-003 -> bd-001 would create a cycle
        let would_cycle = storage
            .would_create_cycle("bd-003", "bd-001", true)
            .unwrap();
        assert!(would_cycle);

        // bd-003 -> bd-002 would also create a cycle
        let would_cycle = storage
            .would_create_cycle("bd-003", "bd-002", true)
            .unwrap();
        assert!(would_cycle);
        info!("test_cycle_detection_transitive: assertions passed");
    }

    #[test]
    fn test_no_false_positive_cycle() {
        init_test_logging();
        info!("test_no_false_positive_cycle: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "Issue 1");
        let issue2 = make_test_issue("bd-002", "Issue 2");
        let issue3 = make_test_issue("bd-003", "Issue 3");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // bd-001 -> bd-002
        storage
            .add_dependency("bd-001", "bd-002", "blocks", "tester")
            .unwrap();

        // bd-003 -> bd-002 should NOT be a cycle
        let would_cycle = storage
            .would_create_cycle("bd-003", "bd-002", true)
            .unwrap();
        assert!(!would_cycle);
        info!("test_no_false_positive_cycle: assertions passed");
    }

    #[test]
    fn test_dep_action_result_json() {
        init_test_logging();
        info!("test_dep_action_result_json: starting");
        let result = DepActionResult {
            status: "ok".to_string(),
            issue_id: "bd-001".to_string(),
            depends_on_id: "bd-002".to_string(),
            dep_type: "blocks".to_string(),
            action: "added".to_string(),
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"issue_id\":\"bd-001\""));
        assert!(json.contains("\"type\":\"blocks\"")); // Note: renamed field
        info!("test_dep_action_result_json: assertions passed");
    }

    #[test]
    fn test_dep_list_item_json() {
        init_test_logging();
        info!("test_dep_list_item_json: starting");
        let item = DepListItem {
            issue_id: "bd-001".to_string(),
            depends_on_id: "bd-002".to_string(),
            dep_type: "blocks".to_string(),
            title: "Test Issue".to_string(),
            status: "open".to_string(),
            priority: 2,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"blocks\"")); // Renamed field
        assert!(json.contains("\"priority\":2"));
        info!("test_dep_list_item_json: assertions passed");
    }

    #[test]
    fn test_sort_dep_list_items_for_human_orders_by_priority() {
        init_test_logging();
        info!("test_sort_dep_list_items_for_human_orders_by_priority: starting");
        let mut items = vec![
            test_dep_list_item("bd-root", "bd-low", 4),
            test_dep_list_item("bd-root", "bd-critical", 0),
            test_dep_list_item("bd-root", "bd-medium", 2),
        ];

        sort_dep_list_items_for_human(&mut items);

        let sorted_ids: Vec<_> = items
            .iter()
            .map(|item| item.depends_on_id.as_str())
            .collect();
        assert_eq!(sorted_ids, ["bd-critical", "bd-medium", "bd-low"]);
        info!("test_sort_dep_list_items_for_human_orders_by_priority: assertions passed");
    }

    #[test]
    fn test_sort_dep_list_items_for_human_uses_ids_as_tiebreakers() {
        init_test_logging();
        info!("test_sort_dep_list_items_for_human_uses_ids_as_tiebreakers: starting");
        let mut items = vec![
            test_dep_list_item("bd-root", "bd-b", 1),
            test_dep_list_item("bd-root", "bd-a", 1),
            test_dep_list_item("bd-root", "bd-c", 1),
        ];

        sort_dep_list_items_for_human(&mut items);

        let sorted_ids: Vec<_> = items
            .iter()
            .map(|item| item.depends_on_id.as_str())
            .collect();
        assert_eq!(sorted_ids, ["bd-a", "bd-b", "bd-c"]);
        info!("test_sort_dep_list_items_for_human_uses_ids_as_tiebreakers: assertions passed");
    }

    #[test]
    fn test_cycles_result_json() {
        init_test_logging();
        info!("test_cycles_result_json: starting");
        let result = CyclesResult {
            cycles: vec![
                vec!["bd-001".to_string(), "bd-002".to_string()],
                vec![
                    "bd-003".to_string(),
                    "bd-004".to_string(),
                    "bd-005".to_string(),
                ],
            ],
            count: 2,
            active_count: 2,
            archived_closed_count: 0,
            total_count: 2,
            blocking_only: false,
            include_closed: false,
            scope: "active",
            active_cycles: Vec::new(),
            archived_closed_cycles: Vec::new(),
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"count\":2"));
        assert!(json.contains("bd-001"));
        info!("test_cycles_result_json: assertions passed");
    }

    #[test]
    fn dep_cycles_human_output_sanitizes_ids_and_omits_literal_markup() {
        let cycles = vec![vec!["bd-a\x1b[2J".to_string(), "bd-b\x07bell".to_string()]];

        let plain = format_cycle_plain(&cycles[0]);
        assert!(!plain.contains('\x1b'));
        assert!(!plain.contains('\x07'));
        assert!(plain.contains("bd-a\\u{1b}[2J -> bd-b\\u{7}bell"));

        let theme = Theme::default();
        let rich_text = build_cycles_rich_text(&cycles, 1, &theme, false);
        let rendered = Panel::from_rich_text(&rich_text, 100).render_plain(100);

        assert!(!rendered.contains("[bold"));
        assert!(!rendered.contains("[red"));
        assert!(!rendered.contains("[/]"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(rendered.contains("bd-a\\u{1b}[2J"));
        assert!(rendered.contains("bd-b\\u{7}bell"));
        assert!(rich_text.spans().len() > 1, "rich text should carry styles");

        let blocking_rich_text = build_cycles_rich_text(&cycles, 1, &theme, true);
        let blocking_rendered = Panel::from_rich_text(&blocking_rich_text, 100).render_plain(100);

        assert!(blocking_rendered.contains("blocking dependency cycle(s)"));
    }

    #[test]
    fn dep_display_and_mermaid_labels_escape_terminal_controls() {
        let display = dep_display_text("bd-a\x1b]52;c;bad\x07");
        assert!(!display.chars().any(char::is_control));
        assert_eq!(display, "bd-a\\u{1b}]52;c;bad\\u{7}");

        let mermaid = sanitize_mermaid_label("bd-a\x1b[2J\n\"quoted\"\r\x07");
        assert!(!mermaid.chars().any(char::is_control));
        assert!(mermaid.contains("bd-a\\u{1b}[2J\\n'quoted'\\r\\u{7}"));
    }

    #[test]
    fn dep_list_human_output_sanitizes_relation_ids() {
        let item = DepListItem {
            issue_id: "bd-parent\x1b[2J".to_string(),
            depends_on_id: "external:proj:\x07cap".to_string(),
            dep_type: "blocks\x1b[type".to_string(),
            title: "Title\x1b[31m".to_string(),
            status: "custom\x07status".to_string(),
            priority: 1,
        };
        let refs = vec![&item];
        let theme = Theme::default();
        let mut content = Text::new("");

        append_dep_list_section(&mut content, "Dependencies (1):", &refs, true, &theme);

        let rendered = content.plain();
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(rendered.contains("external:proj:\\u{7}cap"));
        assert!(rendered.contains("blocks\\u{1b}[type"));
        assert!(rendered.contains("Title\\u{1b}[31m"));
        assert!(rendered.contains("custom\\u{7}status"));

        let title = dep_list_panel_title(DepDirection::Down, "bd-root\x1b[2J");
        assert!(!title.contains('\x1b'));
        assert!(title.contains("bd-root\\u{1b}[2J"));
    }

    #[test]
    fn dep_tree_rich_label_sanitizes_text_and_omits_literal_markup() {
        let node = TreeNode {
            node_key: "n1".to_string(),
            id: "bd-node\x1b[2J".to_string(),
            title: "Tree title\x07bell".to_string(),
            depth: 0,
            parent_id: None,
            parent_key: None,
            priority: 1,
            status: "blocked".to_string(),
            truncated: true,
            repeat: false,
        };
        let theme = Theme::default();
        let label = build_tree_node_label(&node, &theme);
        let rendered = label.plain();

        assert!(!rendered.contains("[red]"));
        assert!(!rendered.contains("[/]"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(rendered.contains("bd-node\\u{1b}[2J"));
        assert!(rendered.contains("Tree title\\u{7}bell"));
        assert!(rendered.contains("[blocked] ⚠"));
        assert!(rendered.contains("(truncated)"));
        assert!(label.spans().len() > 1, "tree label should carry styles");
    }

    #[test]
    fn test_external_dependency_prefix_check() {
        init_test_logging();
        info!("test_external_dependency_prefix_check: starting");
        let external = "external:jira-123";
        assert!(external.starts_with("external:"));

        let normal = "bd-001";
        assert!(!normal.starts_with("external:"));
        info!("test_external_dependency_prefix_check: assertions passed");
    }

    #[test]
    fn test_dep_direction_default() {
        init_test_logging();
        info!("test_dep_direction_default: starting");
        let direction = DepDirection::default();
        assert_eq!(direction, DepDirection::Down);
        info!("test_dep_direction_default: assertions passed");
    }

    #[test]
    fn test_apply_external_dep_list_metadata_sets_status_and_title() {
        init_test_logging();
        info!("test_apply_external_dep_list_metadata_sets_status_and_title: starting");
        let mut items = vec![
            DepListItem {
                issue_id: "bd-001".to_string(),
                depends_on_id: "external:proj:cap".to_string(),
                dep_type: "blocks".to_string(),
                title: String::new(),
                status: "open".to_string(),
                priority: 2,
            },
            DepListItem {
                issue_id: "bd-002".to_string(),
                depends_on_id: "external:proj:cap2".to_string(),
                dep_type: "blocks".to_string(),
                title: String::new(),
                status: "open".to_string(),
                priority: 2,
            },
        ];

        let mut statuses = HashMap::new();
        statuses.insert("external:proj:cap".to_string(), true);
        statuses.insert("external:proj:cap2".to_string(), false);

        apply_external_dep_list_metadata(&mut items, &statuses);

        assert_eq!(items[0].status, "closed");
        assert_eq!(items[0].title, "✓ proj:cap");
        assert_eq!(items[1].status, "blocked");
        assert_eq!(items[1].title, "⏳ proj:cap2");
        info!("test_apply_external_dep_list_metadata_sets_status_and_title: assertions passed");
    }

    #[test]
    fn test_apply_external_dep_list_metadata_preserves_title() {
        init_test_logging();
        info!("test_apply_external_dep_list_metadata_preserves_title: starting");
        let mut items = vec![DepListItem {
            issue_id: "bd-001".to_string(),
            depends_on_id: "external:proj:cap".to_string(),
            dep_type: "blocks".to_string(),
            title: "Already set".to_string(),
            status: "open".to_string(),
            priority: 2,
        }];
        let mut statuses = HashMap::new();
        statuses.insert("external:proj:cap".to_string(), false);

        apply_external_dep_list_metadata(&mut items, &statuses);

        assert_eq!(items[0].status, "blocked");
        assert_eq!(items[0].title, "Already set");
        info!("test_apply_external_dep_list_metadata_preserves_title: assertions passed");
    }

    #[test]
    fn test_apply_external_dep_list_metadata_rewrites_generated_placeholder_title() {
        init_test_logging();
        info!(
            "test_apply_external_dep_list_metadata_rewrites_generated_placeholder_title: starting"
        );
        let mut items = vec![DepListItem {
            issue_id: "bd-001".to_string(),
            depends_on_id: "external:proj:cap".to_string(),
            dep_type: "blocks".to_string(),
            title: "proj:cap".to_string(),
            status: "open".to_string(),
            priority: 2,
        }];
        let mut statuses = HashMap::new();
        statuses.insert("external:proj:cap".to_string(), false);

        apply_external_dep_list_metadata(&mut items, &statuses);

        assert_eq!(items[0].status, "blocked");
        assert_eq!(items[0].title, "⏳ proj:cap");
        info!(
            "test_apply_external_dep_list_metadata_rewrites_generated_placeholder_title: assertions passed"
        );
    }

    #[test]
    fn test_apply_external_dep_list_metadata_external_issue_id() {
        init_test_logging();
        info!("test_apply_external_dep_list_metadata_external_issue_id: starting");
        let mut items = vec![DepListItem {
            issue_id: "external:proj:cap".to_string(),
            depends_on_id: "bd-001".to_string(),
            dep_type: "blocks".to_string(),
            title: String::new(),
            status: "open".to_string(),
            priority: 2,
        }];
        let mut statuses = HashMap::new();
        statuses.insert("external:proj:cap".to_string(), true);

        apply_external_dep_list_metadata(&mut items, &statuses);

        assert_eq!(items[0].status, "closed");
        assert_eq!(items[0].title, "✓ proj:cap");
        info!("test_apply_external_dep_list_metadata_external_issue_id: assertions passed");
    }

    #[test]
    fn test_dep_list_section_title_uses_neutral_dependents_label() {
        init_test_logging();
        info!("test_dep_list_section_title_uses_neutral_dependents_label: starting");
        assert_eq!(dep_list_section_title(true, 2), "Dependencies (2):");
        assert_eq!(dep_list_section_title(false, 3), "Dependents (3):");
        info!("test_dep_list_section_title_uses_neutral_dependents_label: assertions passed");
    }

    #[test]
    fn test_dep_list_panel_title_matches_direction() {
        init_test_logging();
        info!("test_dep_list_panel_title_matches_direction: starting");
        assert_eq!(
            dep_list_panel_title(DepDirection::Down, "bd-1"),
            "Dependencies for bd-1"
        );
        assert_eq!(
            dep_list_panel_title(DepDirection::Up, "bd-1"),
            "Dependents for bd-1"
        );
        assert_eq!(
            dep_list_panel_title(DepDirection::Both, "bd-1"),
            "Dependency relations for bd-1"
        );
        info!("test_dep_list_panel_title_matches_direction: assertions passed");
    }

    #[test]
    fn test_dep_list_status_label_formats_known_statuses() {
        init_test_logging();
        info!("test_dep_list_status_label_formats_known_statuses: starting");
        assert_eq!(dep_list_status_label("open"), "[open]");
        assert_eq!(dep_list_status_label("closed"), "[closed] ✓");
        assert_eq!(dep_list_status_label("custom"), "custom");
        info!("test_dep_list_status_label_formats_known_statuses: assertions passed");
    }

    #[test]
    fn test_dep_tree_truncated_only_when_children_are_omitted() {
        init_test_logging();
        info!("test_dep_tree_truncated_only_when_children_are_omitted: starting");
        assert!(!dep_tree_truncated(2, 2, 0));
        assert!(dep_tree_truncated(2, 2, 1));
        assert!(!dep_tree_truncated(1, 2, 3));
        info!("test_dep_tree_truncated_only_when_children_are_omitted: assertions passed");
    }

    #[test]
    fn test_sort_dep_tree_siblings_uses_metadata_cache() {
        init_test_logging();
        info!("test_sort_dep_tree_siblings_uses_metadata_cache: starting");
        let mut dependencies = vec![
            "bd-low".to_string(),
            "bd-missing".to_string(),
            "bd-active".to_string(),
            "bd-alpha".to_string(),
            "bd-high".to_string(),
        ];
        let mut metadata_cache = HashMap::new();
        metadata_cache.insert(
            "bd-low".to_string(),
            ("Low priority".to_string(), 3, "open".to_string()),
        );
        metadata_cache.insert(
            "bd-high".to_string(),
            ("High priority".to_string(), 0, "open".to_string()),
        );
        metadata_cache.insert(
            "bd-active".to_string(),
            ("Active task".to_string(), 1, "in_progress".to_string()),
        );
        metadata_cache.insert(
            "bd-alpha".to_string(),
            ("Alpha task".to_string(), 1, "open".to_string()),
        );

        sort_dep_tree_siblings(&mut dependencies, &metadata_cache);

        assert_eq!(
            dependencies,
            vec![
                "bd-high".to_string(),
                "bd-alpha".to_string(),
                "bd-active".to_string(),
                "bd-low".to_string(),
                "bd-missing".to_string(),
            ]
        );
        info!("test_sort_dep_tree_siblings_uses_metadata_cache: assertions passed");
    }

    #[test]
    fn test_resolve_dep_tree_node_metadata_missing_internal_issue() {
        init_test_logging();
        info!("test_resolve_dep_tree_node_metadata_missing_internal_issue: starting");
        let storage = SqliteStorage::open_memory().unwrap();
        let root_issue = make_test_issue("bd-root", "Root");
        let statuses = HashMap::new();

        let (title, priority, status) = resolve_dep_tree_node_metadata(
            &storage,
            "bd-root",
            &root_issue,
            "bd-missing",
            &statuses,
        )
        .unwrap();

        assert_eq!(title, "[missing issue: bd-missing]");
        assert_eq!(priority, 2);
        assert_eq!(status, "deleted");
        info!("test_resolve_dep_tree_node_metadata_missing_internal_issue: assertions passed");
    }

    #[test]
    fn test_dep_direction_variants() {
        init_test_logging();
        info!("test_dep_direction_variants: starting");
        assert!(matches!(DepDirection::Down, DepDirection::Down));
        assert!(matches!(DepDirection::Up, DepDirection::Up));
        assert!(matches!(DepDirection::Both, DepDirection::Both));
        info!("test_dep_direction_variants: assertions passed");
    }

    #[test]
    fn nearest_dependency_type_handles_docs_shaped_typos() {
        assert_eq!(
            nearest_dependency_type("parent_child"),
            Some("parent-child")
        );
        assert_eq!(
            nearest_dependency_type("Parent Child"),
            Some("parent-child")
        );
        assert_eq!(nearest_dependency_type("block"), Some("blocks"));
        assert_eq!(nearest_dependency_type("parent"), Some("parent-child"));
        assert_eq!(nearest_dependency_type("relate"), Some("related"));
        assert_eq!(nearest_dependency_type("duplicate"), Some("duplicates"));
        assert_eq!(nearest_dependency_type("bogus"), None);
        assert_eq!(nearest_dependency_type(""), None);
    }

    #[test]
    fn parse_dependency_type_unknown_carries_hint() {
        let err = parse_dependency_type("waits_for").unwrap_err();
        match err {
            BeadsError::ValidationWithHint {
                field,
                reason,
                hint,
            } => {
                assert_eq!(field, "type");
                assert!(reason.contains("Unknown dependency type: 'waits_for'"));
                assert!(reason.contains("caused-by"));
                assert_eq!(hint, "Did you mean --type waits-for?");
            }
            other => panic!("expected ValidationWithHint, got {other:?}"),
        }
        let err = parse_dependency_type("zzz").unwrap_err();
        match err {
            BeadsError::ValidationWithHint { hint, .. } => {
                assert!(hint.starts_with("Use --type with one of: blocks, "));
            }
            other => panic!("expected ValidationWithHint, got {other:?}"),
        }
        assert!(parse_dependency_type("Parent-Child").is_ok());
    }
}
