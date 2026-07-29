//! Delete command implementation.
//!
//! Creates tombstones for issues, handles dependencies, and supports
//! cascade/force/dry-run modes.

use crate::cli::DeleteArgs;
use crate::cli::commands::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, report_auto_flush_failure, resolve_issue_ids,
    retry_mutation_with_jsonl_recovery,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Result of a delete operation for JSON output.
#[derive(Debug, Serialize)]
pub struct DeleteResult {
    pub deleted: Vec<String>,
    pub deleted_count: usize,
    pub dependencies_removed: usize,
    pub labels_removed: usize,
    pub events_removed: usize,
    pub references_updated: usize,
    pub orphaned_issues: Vec<String>,
}

impl DeleteResult {
    const fn new() -> Self {
        Self {
            deleted: Vec::new(),
            deleted_count: 0,
            dependencies_removed: 0,
            labels_removed: 0,
            events_removed: 0,
            references_updated: 0,
            orphaned_issues: Vec::new(),
        }
    }
}

/// JSON output for delete preview / dry-run paths.
#[derive(Debug, Serialize)]
struct DeletePreviewResult {
    preview: bool,
    would_delete: Vec<String>,
    cascade_delete: Vec<String>,
    blocked_dependents: Vec<String>,
    orphaned_issues: Vec<String>,
}

struct PreparedDeleteRoute {
    beads_dir: PathBuf,
    route_cli: config::CliOverrides,
    resolved_ids: Vec<String>,
    blocked_dependents: Vec<String>,
    cascade_delete: Vec<String>,
    final_delete_ids: Vec<String>,
    auto_flush_external: bool,
    _routed_write_lock: RoutedWorkspaceWriteLock,
}

fn delete_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

/// Execute the delete command.
///
/// # Errors
///
/// Returns an error if:
/// - No IDs provided and no --from-file
/// - Issue not found
/// - Has dependents without --force or --cascade
/// - Database operation fails
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &DeleteArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    // 1. Collect IDs from args and/or file
    let mut ids: Vec<String> = args.ids.clone();

    if let Some(ref file_path) = args.from_file {
        let file_ids = read_ids_from_file(file_path)?;
        ids.extend(file_ids);
    }

    if ids.is_empty() {
        // `br delete --hard` with no IDs is the tombstone garbage-collector:
        // purge every tombstone in the current workspace (#367). Plain
        // `br delete` with no IDs remains a usage error.
        if args.hard {
            let beads_dir = config::discover_beads_dir_with_cli(cli)?;
            return purge_all_tombstones(args, cli, ctx, &beads_dir);
        }
        return Err(BeadsError::validation("ids", "no issue IDs provided"));
    }

    // Deduplicate
    let mut ids: Vec<String> = ids
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    ids.sort();

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let routed_batches = config::routing::group_issue_inputs_by_route(&ids, &beads_dir)?;
    if routed_batches.iter().any(|batch| batch.is_external) {
        return execute_routed(args, cli, ctx, &beads_dir, routed_batches);
    }

    // 2. Open storage
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let ids = sorted_unique_strings(resolve_issue_ids(&storage_ctx.storage, &resolver, &ids)?);
    let result = {
        // 3. Check for dependents (if not --force and not --cascade)
        let delete_set: HashSet<String> = ids.iter().cloned().collect();
        let blocked_dependents = collect_direct_dependents(&storage_ctx.storage, &ids)?;
        let cascade_dependents = if args.cascade {
            Some(collect_sorted_cascade_dependents(
                &storage_ctx.storage,
                &ids,
            )?)
        } else {
            None
        };

        if !blocked_dependents.is_empty() && !args.force && !args.cascade {
            // Preview mode: show what would happen.
            // Compute the full transitive closure so the user sees the true blast
            // radius of --cascade (not just first-level dependents).
            let full_cascade: Vec<String> =
                collect_sorted_cascade_dependents(&storage_ctx.storage, &ids)?;
            if ctx.is_json() || ctx.is_toon() {
                let preview = DeletePreviewResult {
                    preview: true,
                    would_delete: ids,
                    cascade_delete: full_cascade,
                    blocked_dependents,
                    orphaned_issues: Vec::new(),
                };
                if ctx.is_toon() {
                    ctx.toon(&preview);
                } else {
                    ctx.json_pretty(&preview);
                }
                return Ok(());
            }
            if ctx.is_quiet() {
                return Ok(());
            }
            if ctx.is_rich() {
                render_dependents_warning_rich(
                    &blocked_dependents,
                    &full_cascade,
                    &storage_ctx.storage,
                    ctx,
                );
            } else {
                println!("The following issues depend on issues being deleted:");
                for dep in &blocked_dependents {
                    println!("  - {}", delete_display_text(dep));
                }
                if full_cascade.len() > blocked_dependents.len() {
                    println!(
                        "\n{} additional issue(s) would be transitively affected by --cascade:",
                        full_cascade.len() - blocked_dependents.len()
                    );
                    let direct_set: HashSet<&String> = blocked_dependents.iter().collect();
                    for dep in &full_cascade {
                        if !direct_set.contains(dep) {
                            println!("  - {}", delete_display_text(dep));
                        }
                    }
                }
                println!();
                println!(
                    "Use --force to orphan these dependents, or --cascade to delete them recursively."
                );
                if !full_cascade.is_empty() {
                    println!(
                        "--cascade would delete {} total dependent(s).",
                        full_cascade.len()
                    );
                }
                println!("No changes made (preview mode).");
            }
            return Ok(());
        }

        // 4. Dry-run mode
        if args.dry_run {
            let cascade_ids = cascade_dependents.clone().unwrap_or_default();
            if ctx.is_json() || ctx.is_toon() {
                let orphaned_issues = if args.force && !args.cascade {
                    blocked_dependents
                } else {
                    Vec::new()
                };
                let preview = DeletePreviewResult {
                    preview: true,
                    would_delete: ids,
                    cascade_delete: cascade_ids,
                    blocked_dependents: Vec::new(),
                    orphaned_issues,
                };
                if ctx.is_toon() {
                    ctx.toon(&preview);
                } else {
                    ctx.json_pretty(&preview);
                }
                return Ok(());
            }
            if ctx.is_quiet() {
                return Ok(());
            }
            if ctx.is_rich() {
                let orphan_ids: Vec<String> = if args.force && !args.cascade {
                    blocked_dependents
                } else {
                    vec![]
                };
                render_dry_run_rich(&ids, &cascade_ids, &orphan_ids, &storage_ctx.storage, ctx);
                return Ok(());
            }
            println!("Dry-run: Would delete {} issue(s):", ids.len());
            for id in &ids {
                let issue = storage_ctx
                    .storage
                    .get_issue(id)?
                    .ok_or_else(|| BeadsError::IssueNotFound { id: id.clone() })?;
                println!(
                    "  - {}: {}",
                    delete_display_text(id),
                    sanitize_terminal_inline(&issue.title)
                );
            }
            if !cascade_ids.is_empty() {
                println!(
                    "Would also cascade delete {} dependent(s):",
                    cascade_ids.len()
                );
                for dep in &cascade_ids {
                    println!("  - {}", delete_display_text(dep));
                }
            }
            if args.force && !blocked_dependents.is_empty() {
                println!("Would orphan {} dependent(s):", blocked_dependents.len());
                for dep in &blocked_dependents {
                    println!("  - {}", delete_display_text(dep));
                }
            }
            return Ok(());
        }

        // 5. Build final delete set
        let mut final_delete_set: HashSet<String> = delete_set;
        if let Some(cascade_ids) = &cascade_dependents {
            final_delete_set.extend(cascade_ids.iter().cloned());
        }

        // 6. Get actor
        let actor = config::resolve_actor(&config_layer);

        // 7. Perform deletion
        let mut result = DeleteResult::new();

        // First, remove all dependency links for issues being deleted
        let mut batch_has_mutated = false;
        for id in &final_delete_set {
            let deps_removed = retry_mutation_with_jsonl_recovery(
                &mut storage_ctx,
                !batch_has_mutated,
                "delete remove dependencies",
                Some(id.as_str()),
                |storage| storage.remove_all_dependencies(id, &actor),
            )?;
            result.dependencies_removed += deps_removed;
            if deps_removed > 0 {
                batch_has_mutated = true;
            }
        }

        // Track orphaned issues (only relevant for --force mode)
        if args.force && !args.cascade {
            result.orphaned_issues.clone_from(&blocked_dependents);
        }

        // Delete each issue (create tombstone, then purge if --hard)
        let mut final_ids: Vec<String> = final_delete_set.into_iter().collect();
        final_ids.sort();
        for id in &final_ids {
            if args.hard {
                result.labels_removed += storage_ctx.storage.get_labels(id)?.len();
                result.events_removed += storage_ctx.storage.count_issue_events(id)?;
                // Hard delete: physically remove from DB so it's pruned from JSONL
                retry_mutation_with_jsonl_recovery(
                    &mut storage_ctx,
                    !batch_has_mutated,
                    "delete purge",
                    Some(id.as_str()),
                    |storage| storage.purge_issue(id, &actor),
                )?;
            } else {
                retry_mutation_with_jsonl_recovery(
                    &mut storage_ctx,
                    !batch_has_mutated,
                    "delete tombstone",
                    Some(id.as_str()),
                    |storage| storage.delete_issue(id, &actor, &args.reason, None),
                )?;
            }
            batch_has_mutated = true;
            result.deleted.push(id.clone());
        }
        result.deleted_count = result.deleted.len();
        result
    };

    let deleted_ids: HashSet<String> = result.deleted.iter().cloned().collect();
    storage_ctx.flush_no_db_then(|ctx| {
        let last_touched = crate::util::get_last_touched_id(&ctx.paths.beads_dir);
        if !last_touched.is_empty() && deleted_ids.contains(&last_touched) {
            crate::util::clear_last_touched(&ctx.paths.beads_dir);
        }
        Ok(())
    })?;

    // 9. Output
    if ctx.is_json() || ctx.is_toon() {
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_rich() {
        render_delete_result_rich(&result, &storage_ctx.storage, ctx);
    } else {
        println!("Deleted {} issue(s):", result.deleted_count);
        for id in &result.deleted {
            println!("  - {}", delete_display_text(id));
        }

        if result.dependencies_removed > 0 {
            println!("Removed {} dependency link(s)", result.dependencies_removed);
        }

        if !result.orphaned_issues.is_empty() {
            println!("Orphaned {} issue(s):", result.orphaned_issues.len());
            for id in &result.orphaned_issues {
                println!("  - {}", delete_display_text(id));
            }
        }
    }

    Ok(())
}

/// Purge every tombstone in the workspace (`br delete --hard` with no IDs).
///
/// Tombstones are the soft-delete markers a normal `br delete` leaves behind so
/// a deletion can propagate through JSONL/git. They accumulate over time and
/// there was previously no way to reclaim them — `br delete --hard` with no IDs
/// errored on the empty argument list (#367). This hard-removes each tombstone
/// row (no new tombstone is written) and prunes it from the JSONL export.
fn purge_all_tombstones(
    args: &DeleteArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);

    let tombstone_ids = storage_ctx.storage.list_tombstone_ids()?;

    if args.dry_run {
        if ctx.is_json() || ctx.is_toon() {
            let preview = DeletePreviewResult {
                preview: true,
                would_delete: tombstone_ids.clone(),
                cascade_delete: Vec::new(),
                blocked_dependents: Vec::new(),
                orphaned_issues: Vec::new(),
            };
            if ctx.is_toon() {
                ctx.toon(&preview);
            } else {
                ctx.json_pretty(&preview);
            }
        } else if !ctx.is_quiet() {
            if tombstone_ids.is_empty() {
                ctx.info("Dry-run: no tombstones to purge.");
            } else {
                println!("Dry-run: Would purge {} tombstone(s):", tombstone_ids.len());
                for id in &tombstone_ids {
                    println!("  - {}", delete_display_text(id));
                }
            }
        }
        return Ok(());
    }

    let mut result = DeleteResult::new();
    let mut batch_has_mutated = false;
    for id in &tombstone_ids {
        result.labels_removed += storage_ctx.storage.get_labels(id)?.len();
        result.events_removed += storage_ctx.storage.count_issue_events(id)?;
        retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            !batch_has_mutated,
            "delete purge",
            Some(id.as_str()),
            |storage| storage.purge_issue(id, &actor),
        )?;
        batch_has_mutated = true;
        result.deleted.push(id.clone());
    }
    result.deleted_count = result.deleted.len();

    let deleted_ids: HashSet<String> = result.deleted.iter().cloned().collect();
    storage_ctx.flush_no_db_then(|ctx| {
        let last_touched = crate::util::get_last_touched_id(&ctx.paths.beads_dir);
        if !last_touched.is_empty() && deleted_ids.contains(&last_touched) {
            crate::util::clear_last_touched(&ctx.paths.beads_dir);
        }
        Ok(())
    })?;

    if ctx.is_json() || ctx.is_toon() {
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if result.deleted_count == 0 {
        ctx.success("No tombstones to purge.");
    } else {
        ctx.success(&format!("Purged {} tombstone(s)", result.deleted_count));
        if !ctx.is_rich() {
            for id in &result.deleted {
                println!("  - {}", delete_display_text(id));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_routed(
    args: &DeleteArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    routed_batches: Vec<config::routing::RoutedIssueBatch>,
) -> Result<()> {
    let normalized_local_beads_dir =
        dunce::canonicalize(local_beads_dir).unwrap_or_else(|_| local_beads_dir.to_path_buf());
    let mut prepared_routes = Vec::with_capacity(routed_batches.len());

    for batch in routed_batches {
        let normalized_batch_beads_dir =
            dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
        let mut batch_cli = cli.clone();
        batch_cli.db = if normalized_batch_beads_dir == normalized_local_beads_dir {
            cli.db.clone()
        } else {
            None
        };
        prepared_routes.push(prepare_delete_route(
            args,
            &batch.issue_inputs,
            &batch.beads_dir,
            &batch_cli,
            batch.is_external,
        )?);
    }

    let would_delete = sorted_unique_strings(
        prepared_routes
            .iter()
            .flat_map(|route| route.resolved_ids.iter().cloned())
            .collect(),
    );
    let blocked_dependents = sorted_unique_strings(
        prepared_routes
            .iter()
            .flat_map(|route| route.blocked_dependents.iter().cloned())
            .collect(),
    );
    let cascade_delete = sorted_unique_strings(
        prepared_routes
            .iter()
            .flat_map(|route| route.cascade_delete.iter().cloned())
            .collect(),
    );

    if !blocked_dependents.is_empty() && !args.force && !args.cascade {
        render_routed_delete_preview(
            ctx,
            &DeletePreviewResult {
                preview: true,
                would_delete,
                cascade_delete,
                blocked_dependents,
                orphaned_issues: Vec::new(),
            },
        );
        return Ok(());
    }

    if args.dry_run {
        let orphaned_issues = if args.force && !args.cascade {
            blocked_dependents
        } else {
            Vec::new()
        };
        let cascade_delete = if args.cascade {
            cascade_delete
        } else {
            Vec::new()
        };
        render_routed_delete_preview(
            ctx,
            &DeletePreviewResult {
                preview: true,
                would_delete,
                cascade_delete,
                blocked_dependents: Vec::new(),
                orphaned_issues,
            },
        );
        return Ok(());
    }

    let mut result = DeleteResult::new();
    for route in &prepared_routes {
        let batch_result = apply_delete_route(args, route, ctx)?;
        merge_delete_result(&mut result, batch_result);
    }
    finalize_delete_result(&mut result);
    clear_last_touched_if_deleted(local_beads_dir, &result.deleted);

    if ctx.is_json() || ctx.is_toon() {
        if ctx.is_toon() {
            ctx.toon(&result);
        } else {
            ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    ctx.success(&format!("Deleted {} issue(s)", result.deleted_count));
    for id in &result.deleted {
        ctx.print_line(&format!("  - {}", delete_display_text(id)));
    }

    if result.dependencies_removed > 0 {
        ctx.info(&format!(
            "Removed {} dependency link(s)",
            result.dependencies_removed
        ));
    }

    if !result.orphaned_issues.is_empty() {
        ctx.warning(&format!(
            "Orphaned {} issue(s):",
            result.orphaned_issues.len()
        ));
        for id in &result.orphaned_issues {
            ctx.print_line(&format!("  - {}", delete_display_text(id)));
        }
    }

    Ok(())
}

fn prepare_delete_route(
    args: &DeleteArgs,
    issue_inputs: &[String],
    beads_dir: &Path,
    cli: &config::CliOverrides,
    auto_flush_external: bool,
) -> Result<PreparedDeleteRoute> {
    let routed_write_lock =
        acquire_routed_workspace_write_lock(beads_dir, auto_flush_external, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let resolved_ids = sorted_unique_strings(resolve_issue_ids(
        &storage_ctx.storage,
        &resolver,
        issue_inputs,
    )?);
    let blocked_dependents = collect_direct_dependents(&storage_ctx.storage, &resolved_ids)?;
    let cascade_delete = collect_sorted_cascade_dependents(&storage_ctx.storage, &resolved_ids)?;
    let final_delete_ids = if args.cascade {
        sorted_unique_strings(
            resolved_ids
                .iter()
                .chain(cascade_delete.iter())
                .cloned()
                .collect(),
        )
    } else {
        resolved_ids.clone()
    };

    Ok(PreparedDeleteRoute {
        beads_dir: beads_dir.to_path_buf(),
        route_cli: cli.clone(),
        resolved_ids,
        blocked_dependents,
        cascade_delete,
        final_delete_ids,
        auto_flush_external,
        _routed_write_lock: routed_write_lock,
    })
}

fn apply_delete_route(
    args: &DeleteArgs,
    route: &PreparedDeleteRoute,
    ctx: &OutputContext,
) -> Result<DeleteResult> {
    let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route.route_cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, &route.route_cli)?;
    let config_layer = storage_ctx.load_config(&route.route_cli)?;
    let actor = config::resolve_actor(&config_layer);

    let mut result = DeleteResult::new();
    let mut batch_has_mutated = false;
    for id in &route.final_delete_ids {
        let deps_removed = retry_mutation_with_jsonl_recovery(
            &mut storage_ctx,
            !batch_has_mutated,
            "delete remove dependencies",
            Some(id.as_str()),
            |storage| storage.remove_all_dependencies(id, &actor),
        )?;
        result.dependencies_removed += deps_removed;
        if deps_removed > 0 {
            batch_has_mutated = true;
        }
    }

    if args.force && !args.cascade {
        result.orphaned_issues.clone_from(&route.blocked_dependents);
    }

    for id in &route.final_delete_ids {
        if args.hard {
            result.labels_removed += storage_ctx.storage.get_labels(id)?.len();
            result.events_removed += storage_ctx.storage.count_issue_events(id)?;
            retry_mutation_with_jsonl_recovery(
                &mut storage_ctx,
                !batch_has_mutated,
                "delete purge",
                Some(id.as_str()),
                |storage| storage.purge_issue(id, &actor),
            )?;
        } else {
            retry_mutation_with_jsonl_recovery(
                &mut storage_ctx,
                !batch_has_mutated,
                "delete tombstone",
                Some(id.as_str()),
                |storage| storage.delete_issue(id, &actor, &args.reason, None),
            )?;
        }
        batch_has_mutated = true;
        result.deleted.push(id.clone());
    }
    result.deleted_count = result.deleted.len();

    let deleted_ids: HashSet<String> = result.deleted.iter().cloned().collect();
    storage_ctx.flush_no_db_then(|ctx| {
        let last_touched = crate::util::get_last_touched_id(&ctx.paths.beads_dir);
        if !last_touched.is_empty() && deleted_ids.contains(&last_touched) {
            crate::util::clear_last_touched(&ctx.paths.beads_dir);
        }
        Ok(())
    })?;
    if route.auto_flush_external
        && let Err(error) = storage_ctx.auto_flush_if_enabled()
    {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }

    Ok(result)
}

fn render_routed_delete_preview(ctx: &OutputContext, preview: &DeletePreviewResult) {
    if ctx.is_json() || ctx.is_toon() {
        if ctx.is_toon() {
            ctx.toon(&preview);
        } else {
            ctx.json_pretty(&preview);
        }
        return;
    }

    if ctx.is_quiet() {
        return;
    }

    if !preview.blocked_dependents.is_empty() {
        ctx.warning("The following issues depend on issues being deleted:");
        for dep in &preview.blocked_dependents {
            ctx.print_line(&format!("  - {}", delete_display_text(dep)));
        }
        if preview.cascade_delete.len() > preview.blocked_dependents.len() {
            ctx.newline();
            ctx.info(&format!(
                "{} additional issue(s) would be transitively affected by --cascade:",
                preview.cascade_delete.len() - preview.blocked_dependents.len()
            ));
            let direct_set: HashSet<&String> = preview.blocked_dependents.iter().collect();
            for dep in &preview.cascade_delete {
                if !direct_set.contains(dep) {
                    ctx.print_line(&format!("  - {}", delete_display_text(dep)));
                }
            }
        }
        ctx.newline();
        ctx.info(
            "Use --force to orphan these dependents, or --cascade to delete them recursively.",
        );
        if !preview.cascade_delete.is_empty() {
            ctx.info(&format!(
                "--cascade would delete {} total dependent(s).",
                preview.cascade_delete.len()
            ));
        }
        ctx.info("No changes made (preview mode).");
        return;
    }

    ctx.info(&format!(
        "Dry-run: Would delete {} issue(s):",
        preview.would_delete.len()
    ));
    for id in &preview.would_delete {
        ctx.print_line(&format!("  - {}", delete_display_text(id)));
    }
    if !preview.cascade_delete.is_empty() {
        ctx.info(&format!(
            "Would also cascade delete {} dependent(s):",
            preview.cascade_delete.len()
        ));
        for dep in &preview.cascade_delete {
            ctx.print_line(&format!("  - {}", delete_display_text(dep)));
        }
    }
    if !preview.orphaned_issues.is_empty() {
        ctx.warning(&format!(
            "Would orphan {} dependent(s):",
            preview.orphaned_issues.len()
        ));
        for dep in &preview.orphaned_issues {
            ctx.print_line(&format!("  - {}", delete_display_text(dep)));
        }
    }
}

fn merge_delete_result(result: &mut DeleteResult, mut batch_result: DeleteResult) {
    result.deleted.append(&mut batch_result.deleted);
    result.dependencies_removed += batch_result.dependencies_removed;
    result.labels_removed += batch_result.labels_removed;
    result.events_removed += batch_result.events_removed;
    result.references_updated += batch_result.references_updated;
    result
        .orphaned_issues
        .append(&mut batch_result.orphaned_issues);
}

fn finalize_delete_result(result: &mut DeleteResult) {
    result.deleted = sorted_unique_strings(std::mem::take(&mut result.deleted));
    result.orphaned_issues = sorted_unique_strings(std::mem::take(&mut result.orphaned_issues));
    result.deleted_count = result.deleted.len();
}

fn clear_last_touched_if_deleted(beads_dir: &Path, deleted_ids: &[String]) {
    let deleted_ids: HashSet<&str> = deleted_ids.iter().map(String::as_str).collect();
    let last_touched = crate::util::get_last_touched_id(beads_dir);
    if !last_touched.is_empty() && deleted_ids.contains(last_touched.as_str()) {
        crate::util::clear_last_touched(beads_dir);
    }
}

fn sorted_unique_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

/// Read issue IDs from a file (one per line, # comments ignored).
fn read_ids_from_file(path: &Path) -> Result<Vec<String>> {
    let file = fs::File::open(path)?;

    let reader = BufReader::new(file);
    let mut ids = Vec::new();

    for line in reader.lines() {
        let line = line?;

        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        ids.push(trimmed.to_string());
    }

    Ok(ids)
}

/// Recursively collect all blocked issues for cascade deletion.
fn collect_cascade_dependents(
    storage: &SqliteStorage,
    initial_ids: &[String],
) -> Result<HashSet<String>> {
    let mut all_ids: HashSet<String> = initial_ids.iter().cloned().collect();
    let mut to_process: Vec<String> = initial_ids.to_vec();

    while let Some(id) = to_process.pop() {
        let dependents = storage.get_blocked_issue_ids(&id)?;
        for dep_id in dependents {
            if all_ids.insert(dep_id.clone()) {
                // New ID, add to processing queue
                to_process.push(dep_id);
            }
        }
    }

    // Remove the initial IDs (they're handled separately)
    for id in initial_ids {
        all_ids.remove(id);
    }

    Ok(all_ids)
}

fn collect_direct_dependents(
    storage: &SqliteStorage,
    initial_ids: &[String],
) -> Result<Vec<String>> {
    let delete_set: HashSet<String> = initial_ids.iter().cloned().collect();
    let mut dependents = Vec::new();

    for id in initial_ids {
        let direct_dependents = storage.get_blocked_issue_ids(id)?;
        for dep_id in direct_dependents {
            if !delete_set.contains(&dep_id) {
                dependents.push(dep_id);
            }
        }
    }

    dependents.sort();
    dependents.dedup();
    Ok(dependents)
}

fn collect_sorted_cascade_dependents(
    storage: &SqliteStorage,
    initial_ids: &[String],
) -> Result<Vec<String>> {
    let mut cascade_ids: Vec<String> = collect_cascade_dependents(storage, initial_ids)?
        .into_iter()
        .collect();
    cascade_ids.sort();
    Ok(cascade_ids)
}

/// Render the dependents warning panel in rich format.
fn render_dependents_warning_rich(
    direct_dependents: &[String],
    full_cascade: &[String],
    storage: &SqliteStorage,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut all_ids: Vec<&String> = direct_dependents
        .iter()
        .chain(full_cascade.iter())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    let all_ids_owned: Vec<String> = all_ids.into_iter().cloned().collect();
    let mut issues_by_id = std::collections::HashMap::new();
    if let Ok(issues) = storage.get_issues_by_ids(&all_ids_owned) {
        for issue in issues {
            issues_by_id.insert(issue.id.clone(), issue);
        }
    }

    let mut content = Text::new("");

    content.append_styled(
        "The following issues depend on issues being deleted:\n\n",
        theme.warning.clone(),
    );

    for dep_id in direct_dependents {
        content.append_styled("  \u{2022} ", theme.dimmed.clone());
        let display_id = delete_display_text(dep_id);
        content.append_styled(&display_id, theme.issue_id.clone());
        if let Some(issue) = issues_by_id.get(dep_id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(sanitize_terminal_inline(&issue.title).as_ref());
        }
        content.append("\n");
    }

    // Show transitive dependents if the full cascade set is larger
    let direct_set: HashSet<&String> = direct_dependents.iter().collect();
    let transitive: Vec<&String> = full_cascade
        .iter()
        .filter(|id| !direct_set.contains(id))
        .collect();
    if !transitive.is_empty() {
        content.append("\n");
        content.append_styled(
            &format!(
                "{} additional issue(s) transitively affected by --cascade:\n\n",
                transitive.len()
            ),
            theme.warning.clone(),
        );
        for dep_id in &transitive {
            content.append_styled("  \u{21b3} ", theme.warning.clone());
            let display_id = delete_display_text(dep_id);
            content.append_styled(&display_id, theme.issue_id.clone());
            if let Some(issue) = issues_by_id.get(*dep_id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(sanitize_terminal_inline(&issue.title).as_ref());
            }
            content.append("\n");
        }
    }

    content.append("\n");
    content.append_styled(
        "Use --force to orphan these dependents, or --cascade to delete them recursively.\n",
        theme.dimmed.clone(),
    );
    if !full_cascade.is_empty() {
        content.append_styled(
            &format!(
                "--cascade would delete {} total dependent(s).\n",
                full_cascade.len()
            ),
            theme.dimmed.clone(),
        );
    }
    content.append_styled("No changes made (preview mode).", theme.muted.clone());

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "\u{26a0} Blocked by Dependents",
            theme.warning.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render the dry-run preview in rich format.
fn render_dry_run_rich(
    ids: &[String],
    cascade_ids: &[String],
    orphan_ids: &[String],
    storage: &SqliteStorage,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut all_ids = Vec::new();
    all_ids.extend_from_slice(ids);
    all_ids.extend_from_slice(cascade_ids);
    all_ids.extend_from_slice(orphan_ids);
    let mut issues_by_id = std::collections::HashMap::new();
    if let Ok(issues) = storage.get_issues_by_ids(&all_ids) {
        for issue in issues {
            issues_by_id.insert(issue.id.clone(), issue);
        }
    }

    let mut content = Text::new("");

    // Main issues to delete
    content.append_styled("Would delete ", theme.dimmed.clone());
    content.append_styled(&format!("{}", ids.len()), theme.emphasis.clone());
    content.append_styled(" issue(s):\n\n", theme.dimmed.clone());

    for id in ids {
        content.append_styled("  \u{2717} ", theme.error.clone());
        let display_id = delete_display_text(id);
        content.append_styled(&display_id, theme.issue_id.clone());
        if let Some(issue) = issues_by_id.get(id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(sanitize_terminal_inline(&issue.title).as_ref());
        }
        content.append("\n");
    }

    // Cascade section
    if !cascade_ids.is_empty() {
        content.append("\n");
        content.append_styled("Would cascade delete ", theme.dimmed.clone());
        content.append_styled(&format!("{}", cascade_ids.len()), theme.emphasis.clone());
        content.append_styled(" dependent(s):\n\n", theme.dimmed.clone());

        for id in cascade_ids {
            content.append_styled("  \u{21b3} ", theme.warning.clone());
            let display_id = delete_display_text(id);
            content.append_styled(&display_id, theme.issue_id.clone());
            if let Some(issue) = issues_by_id.get(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(sanitize_terminal_inline(&issue.title).as_ref());
            }
            content.append("\n");
        }
    }

    // Orphan section
    if !orphan_ids.is_empty() {
        content.append("\n");
        content.append_styled("Would orphan ", theme.dimmed.clone());
        content.append_styled(&format!("{}", orphan_ids.len()), theme.emphasis.clone());
        content.append_styled(" dependent(s):\n\n", theme.dimmed.clone());

        for id in orphan_ids {
            content.append_styled("  \u{26a0} ", theme.warning.clone());
            let display_id = delete_display_text(id);
            content.append_styled(&display_id, theme.issue_id.clone());
            if let Some(issue) = issues_by_id.get(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(sanitize_terminal_inline(&issue.title).as_ref());
            }
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "\u{1f4cb} Dry Run Preview",
            theme.info.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render the delete result in rich format.
fn render_delete_result_rich(result: &DeleteResult, storage: &SqliteStorage, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut all_ids = Vec::new();
    all_ids.extend_from_slice(&result.deleted);
    all_ids.extend_from_slice(&result.orphaned_issues);
    let mut issues_by_id = std::collections::HashMap::new();
    if let Ok(issues) = storage.get_issues_by_ids(&all_ids) {
        for issue in issues {
            issues_by_id.insert(issue.id.clone(), issue);
        }
    }

    let mut content = Text::new("");

    // Deleted items
    content.append_styled("Deleted ", theme.success.clone());
    content.append_styled(&format!("{}", result.deleted_count), theme.emphasis.clone());
    content.append_styled(" issue(s):\n\n", theme.success.clone());

    for id in &result.deleted {
        content.append_styled("  \u{2713} ", theme.success.clone());
        let display_id = delete_display_text(id);
        content.append_styled(&display_id, theme.issue_id.clone());
        if let Some(issue) = issues_by_id.get(id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(sanitize_terminal_inline(&issue.title).as_ref());
        }
        content.append("\n");
    }

    // Dependencies removed
    if result.dependencies_removed > 0 {
        content.append("\n");
        content.append_styled("Removed ", theme.dimmed.clone());
        content.append_styled(
            &format!("{}", result.dependencies_removed),
            theme.emphasis.clone(),
        );
        content.append_styled(" dependency link(s)", theme.dimmed.clone());
    }

    // Orphaned issues
    if !result.orphaned_issues.is_empty() {
        content.append("\n\n");
        content.append_styled("Orphaned ", theme.warning.clone());
        content.append_styled(
            &format!("{}", result.orphaned_issues.len()),
            theme.emphasis.clone(),
        );
        content.append_styled(" issue(s):\n\n", theme.warning.clone());

        for id in &result.orphaned_issues {
            content.append_styled("  \u{26a0} ", theme.warning.clone());
            let display_id = delete_display_text(id);
            content.append_styled(&display_id, theme.issue_id.clone());
            if let Some(issue) = issues_by_id.get(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(sanitize_terminal_inline(&issue.title).as_ref());
            }
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "\u{1f5d1} Delete Complete",
            theme.success.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::Utc;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    fn create_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            content_hash: None,
            description: None,
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
    fn delete_display_text_sanitizes_terminal_controls() {
        let display = delete_display_text("bd-\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!display.chars().any(char::is_control));
        assert!(display.contains("\\u{1b}[2J"));
        assert!(display.contains("\\r"));
        assert!(display.contains("\\u{8}"));
        assert!(display.contains("\\n"));
        assert!(display.contains("\\u{7}"));
        assert!(display.contains("\\u{9b}"));
    }

    #[test]
    fn test_read_ids_from_file() {
        init_logging();
        info!("test_read_ids_from_file: starting");
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "bd-1").unwrap();
        writeln!(file, "# comment").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "bd-2").unwrap();
        writeln!(file, "  bd-3  ").unwrap();
        file.flush().unwrap();

        let ids = read_ids_from_file(file.path()).unwrap();
        assert_eq!(ids, vec!["bd-1", "bd-2", "bd-3"]);
        info!("test_read_ids_from_file: assertions passed");
    }

    #[test]
    fn test_delete_creates_tombstone() {
        init_logging();
        info!("test_delete_creates_tombstone: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = create_test_issue("bd-del1", "Test Delete");
        storage.create_issue(&issue, "tester").unwrap();

        // Verify issue exists
        let before = storage.get_issue("bd-del1").unwrap().unwrap();
        assert_eq!(before.status, Status::Open);

        // Delete it
        let deleted = storage
            .delete_issue("bd-del1", "tester", "test deletion", None)
            .unwrap();
        assert_eq!(deleted.status, Status::Tombstone);
        assert!(deleted.deleted_at.is_some());
        assert_eq!(deleted.deleted_by.as_deref(), Some("tester"));
        assert_eq!(deleted.delete_reason.as_deref(), Some("test deletion"));
        assert_eq!(deleted.original_type.as_deref(), Some("task"));
        info!("test_delete_creates_tombstone: assertions passed");
    }

    #[test]
    fn test_delete_nonexistent_fails() {
        init_logging();
        info!("test_delete_nonexistent_fails: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();
        let result = storage.delete_issue("bd-nope", "tester", "reason", None);
        assert!(result.is_err());
        info!("test_delete_nonexistent_fails: assertions passed");
    }

    #[test]
    fn test_cascade_dependents_collection() {
        init_logging();
        info!("test_cascade_dependents_collection: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Create issues: A -> B -> C (C depends on B, B depends on A)
        let a = create_test_issue("bd-a", "Issue A");
        let b = create_test_issue("bd-b", "Issue B");
        let c = create_test_issue("bd-c", "Issue C");

        storage.create_issue(&a, "tester").unwrap();
        storage.create_issue(&b, "tester").unwrap();
        storage.create_issue(&c, "tester").unwrap();

        // Add dependencies
        storage
            .mutate("test_add_deps", "tester", |tx, _ctx| {
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("bd-a"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-c"), fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                Ok(())
            })
            .unwrap();

        // Collect cascade from A
        let cascade = collect_cascade_dependents(&storage, &["bd-a".to_string()]).unwrap();
        assert!(cascade.contains("bd-b"));
        assert!(cascade.contains("bd-c"));
        assert!(!cascade.contains("bd-a")); // Initial ID not included
        info!("test_cascade_dependents_collection: assertions passed");
    }

    #[test]
    fn test_direct_dependents_collection_is_shallow() {
        init_logging();
        info!("test_direct_dependents_collection_is_shallow: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let a = create_test_issue("bd-a", "Issue A");
        let b = create_test_issue("bd-b", "Issue B");
        let c = create_test_issue("bd-c", "Issue C");

        storage.create_issue(&a, "tester").unwrap();
        storage.create_issue(&b, "tester").unwrap();
        storage.create_issue(&c, "tester").unwrap();

        storage
            .mutate("test_add_direct_deps", "tester", |tx, _ctx| {
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("bd-a"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-c"), fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                Ok(())
            })
            .unwrap();

        let direct = collect_direct_dependents(&storage, &["bd-a".to_string()]).unwrap();
        assert_eq!(direct, vec!["bd-b".to_string()]);
        info!("test_direct_dependents_collection_is_shallow: assertions passed");
    }

    #[test]
    fn test_sorted_cascade_dependents_include_transitive_dependents() {
        init_logging();
        info!("test_sorted_cascade_dependents_include_transitive_dependents: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let a = create_test_issue("bd-a", "Issue A");
        let b = create_test_issue("bd-b", "Issue B");
        let c = create_test_issue("bd-c", "Issue C");

        storage.create_issue(&a, "tester").unwrap();
        storage.create_issue(&b, "tester").unwrap();
        storage.create_issue(&c, "tester").unwrap();

        storage
            .mutate("test_add_transitive_deps", "tester", |tx, _ctx| {
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("bd-a"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-c"), fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                Ok(())
            })
            .unwrap();

        let cascade = collect_sorted_cascade_dependents(&storage, &["bd-a".to_string()]).unwrap();
        assert_eq!(cascade, vec!["bd-b".to_string(), "bd-c".to_string()]);
        info!("test_sorted_cascade_dependents_include_transitive_dependents: assertions passed");
    }

    #[test]
    fn test_preview_cascade_closure_exceeds_direct_dependents() {
        init_logging();
        info!("test_preview_cascade_closure_exceeds_direct_dependents: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Chain: A → B → C → D (D depends on C, C depends on B, B depends on A)
        let a = create_test_issue("bd-a", "Issue A");
        let b = create_test_issue("bd-b", "Issue B");
        let c = create_test_issue("bd-c", "Issue C");
        let d = create_test_issue("bd-d", "Issue D");

        storage.create_issue(&a, "tester").unwrap();
        storage.create_issue(&b, "tester").unwrap();
        storage.create_issue(&c, "tester").unwrap();
        storage.create_issue(&d, "tester").unwrap();

        storage
            .mutate("test_add_chain_deps", "tester", |tx, _ctx| {
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("bd-a"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-c"), fsqlite_types::SqliteValue::from("bd-b"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                tx.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    &[fsqlite_types::SqliteValue::from("bd-d"), fsqlite_types::SqliteValue::from("bd-c"), fsqlite_types::SqliteValue::from("blocks"), fsqlite_types::SqliteValue::from(chrono::Utc::now().to_rfc3339().as_str())],
                )?;
                Ok(())
            })
            .unwrap();

        // Direct dependents of A: only B (first-level)
        let direct = collect_direct_dependents(&storage, &["bd-a".to_string()]).unwrap();
        assert_eq!(direct, vec!["bd-b".to_string()]);

        // Full cascade closure: B, C, and D (the complete transitive set)
        let cascade = collect_sorted_cascade_dependents(&storage, &["bd-a".to_string()]).unwrap();
        assert_eq!(
            cascade,
            vec!["bd-b".to_string(), "bd-c".to_string(), "bd-d".to_string()]
        );

        // The cascade closure MUST be a strict superset of direct dependents
        // This is the invariant that the preview must reflect
        assert!(cascade.len() > direct.len());
        for dep in &direct {
            assert!(cascade.contains(dep));
        }
        info!("test_preview_cascade_closure_exceeds_direct_dependents: assertions passed");
    }
}
