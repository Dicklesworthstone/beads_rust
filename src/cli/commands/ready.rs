//! Ready command implementation.
//!
//! Shows issues ready to work on next: open, unblocked, not deferred, not pinned,
//! not ephemeral.

use super::{auto_import_external_projects_if_stale, resolve_issue_id};
use crate::cli::{
    OutputFormat, ReadyArgs, SortPolicy, resolve_output_format_basic_with_outer_mode,
};
use crate::config;
use crate::error::Result;
use crate::format::{
    ReadyIssue, format_priority_badge, format_type_badge, terminal_width, truncate_title,
};
use crate::model::{IssueType, Priority};
use crate::output::{IssueTable, IssueTableColumns, OutputContext, OutputMode};
use crate::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use std::io::IsTerminal;
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, info, trace};
use unicode_width::UnicodeWidthStr;

/// Execute the ready command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &ReadyArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    execute_inner(args, cli, outer_ctx, &beads_dir, None, None)
}

/// Execute ready using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if configuration loading or the ready query fails.
pub fn execute_with_storage(
    args: &ReadyArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage: &SqliteStorage,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, beads_dir, Some(storage), None)
}

/// Execute ready using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error if configuration loading or the ready query fails.
pub fn execute_with_storage_ctx(
    args: &ReadyArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, beads_dir, None, Some(storage_ctx))
}

#[allow(clippy::too_many_lines)]
fn execute_inner(
    args: &ReadyArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    let owned_storage_ctx = if preloaded_storage.is_some() || preloaded_storage_ctx.is_some() {
        None
    } else {
        Some(config::open_storage_with_cli(beads_dir, cli)?)
    };
    let storage = preloaded_storage
        .or_else(|| preloaded_storage_ctx.map(|ctx| &ctx.storage))
        .or_else(|| owned_storage_ctx.as_ref().map(|ctx| &ctx.storage))
        .expect("ready should have an open storage handle");
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        args.robot,
    );
    let quiet = cli.quiet.unwrap_or(false);
    let early_ctx = OutputContext::from_output_format(output_format, quiet, true);
    let mut config_layer: Option<config::ConfigLayer> = None;
    let mut load_config_layer = || -> Result<config::ConfigLayer> {
        if let Some(layer) = config_layer.as_ref() {
            return Ok(layer.clone());
        }

        let loaded = if let Some(storage_ctx) = preloaded_storage_ctx.or(owned_storage_ctx.as_ref())
        {
            storage_ctx.load_config(cli)?
        } else {
            config::load_config(beads_dir, Some(storage), cli)?
        };
        config_layer = Some(loaded.clone());
        Ok(loaded)
    };
    let assignee = match args.assignee.as_deref() {
        Some("") => Some(config::resolve_actor(&load_config_layer()?)),
        Some(value) => Some(value.to_string()),
        None => None,
    };

    // `--epic <id>` is sugar for `--parent <id> --recursive` (they conflict at
    // the clap layer, so at most one of these is set).
    let parent_spec = args.epic.as_deref().or(args.parent.as_deref());
    let resolved_parent = parent_spec
        .map(|parent| {
            let config_layer = load_config_layer()?;
            let id_config = config::id_config_from_layer(&config_layer);
            let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
            resolve_issue_id(storage, &resolver, parent)
        })
        .transpose()?;

    // Resolve the configured "ready" status group (#354). Defaults to `[open]`
    // when `.beads/policy.yaml` has no `workflow.status_groups.ready`, which is
    // a zero-behavior-change for existing repos. Strict-mode validation rejects
    // an out-of-vocabulary group before we ever query.
    let workflow = crate::close_policy::load_for_beads_dir(beads_dir)?.workflow;
    workflow.validate_ready_status_group()?;
    let ready_statuses = workflow.ready_status_group();

    let filters = ReadyFilters {
        assignee,
        unassigned: args.unassigned,
        labels_and: args.label.clone(),
        labels_or: args.label_any.clone(),
        types: parse_types(&args.type_)?,
        priorities: parse_priorities(&args.priority)?,
        include_deferred: args.include_deferred,
        ready_statuses,
        // Fetch all candidates to allow post-filtering of external blockers
        limit: None,
        parent: resolved_parent,
        // --epic implies descent through the whole subtree.
        recursive: args.recursive || args.epic.is_some(),
        parent_member_ids: None,
    };

    let sort_policy = match args.sort {
        SortPolicy::Hybrid => ReadySortPolicy::Hybrid,
        SortPolicy::Priority => ReadySortPolicy::Priority,
        SortPolicy::Oldest => ReadySortPolicy::Oldest,
    };

    info!("Fetching ready issues");

    // Fetch the full ready set (no SQL LIMIT) so we always know the exact total
    // before truncation — this lets us emit an accurate "showing N of M" note
    // when `--limit` actually truncates, consistent with `br list` and the MCP
    // ready surface (which prints "N total, showing top M"). See issue #91:
    // results must never be *silently* truncated.
    let mut filters = filters;
    filters.limit = None;

    debug!(filters = ?filters, sort = ?sort_policy, "Applied ready filters");

    let mut ready_issues =
        get_ready_issues_for_output(storage, &filters, sort_policy, output_format)?;

    if !ready_issues.is_empty() && storage.has_external_dependencies(true)? {
        let config_layer = load_config_layer()?;
        auto_import_external_projects_if_stale(&config_layer, beads_dir, cli);
        let external_db_paths = config::external_project_db_paths(&config_layer, beads_dir);
        let external_statuses =
            storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
        let external_blockers = storage.external_blockers(&external_statuses)?;
        if !external_blockers.is_empty() {
            ready_issues.retain(|issue| !external_blockers.contains_key(&issue.id));
        }
    }

    // Apply the user-visible limit in Rust (after external-blocker filtering),
    // recording the true pre-truncation total so the text surface can report it.
    let total_before_truncation = ready_issues.len();
    let truncated = args.limit > 0 && ready_issues.len() > args.limit;
    if truncated {
        ready_issues.truncate(args.limit);
    }

    info!(count = ready_issues.len(), "Found ready issues");
    for issue in ready_issues.iter().take(5) {
        trace!(id = %issue.id, priority = issue.priority.0, "Ready issue");
    }

    // Output
    if matches!(early_ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }
    match output_format {
        OutputFormat::Json => {
            hydrate_ready_labels(storage, &mut ready_issues)?;
            early_ctx.json_array(ready_issues.into_iter().map(ReadyIssue::from));
        }
        OutputFormat::Toon => {
            hydrate_ready_labels(storage, &mut ready_issues)?;
            let ready_output: Vec<ReadyIssue> =
                ready_issues.into_iter().map(ReadyIssue::from).collect();
            early_ctx.toon_with_stats(&ready_output, args.stats);
        }
        OutputFormat::Text | OutputFormat::Csv => {
            let config_layer = load_config_layer()?;
            let use_color = config::should_use_color(&config_layer);
            let max_width = if std::io::stdout().is_terminal() {
                Some(terminal_width())
            } else {
                None
            };
            let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);
            if ready_issues.is_empty() {
                println!("{}", empty_ready_message(storage)?);
            } else if matches!(ctx.mode(), OutputMode::Rich) {
                let columns = IssueTableColumns {
                    id: true,
                    priority: true,
                    status: true,
                    issue_type: true,
                    title: true,
                    ..Default::default()
                };
                let mut table = IssueTable::new(&ready_issues, ctx.theme())
                    .columns(columns)
                    .title(format!(
                        "Ready work ({} issue{} with no blockers)",
                        ready_issues.len(),
                        if ready_issues.len() == 1 { "" } else { "s" }
                    ))
                    .wrap(args.wrap);
                if args.wrap {
                    table = table.width(Some(ctx.width()));
                }
                let table = table.build();
                ctx.render(&table);
            } else {
                // Match bd header format: 📋 Ready work (N issues with no blockers):
                println!(
                    "📋 Ready work ({} issue{} with no blockers):\n",
                    ready_issues.len(),
                    if ready_issues.len() == 1 { "" } else { "s" }
                );
                for (i, issue) in ready_issues.iter().enumerate() {
                    let line = format_ready_line(i + 1, issue, use_color, max_width, args.wrap);
                    println!("{line}");
                }
            }

            // Surface truncation explicitly so the top-priority rows filling the
            // limit never read as "queue drained" (#91, #356). Mirrors the
            // `br list` note and the MCP ready surface's "N total, showing top M".
            if truncated && !quiet {
                eprintln!(
                    "[note] Showing {} of {} ready issues. Use --limit 0 for all results.",
                    ready_issues.len(),
                    total_before_truncation,
                );
            }
        }
    }

    Ok(())
}

/// Populate `labels` on ready issues for structured output (#309).
///
/// The ready candidate query hydrates only the columns stored directly on the
/// `issues` row; labels live in a separate table, so JSON/TOON consumers need a
/// single extra batched lookup to get full parity with `br list --json`.
fn hydrate_ready_labels(storage: &SqliteStorage, issues: &mut [crate::model::Issue]) -> Result<()> {
    if issues.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let mut labels_by_id = storage.get_labels_for_issues(&ids)?;
    for issue in issues.iter_mut() {
        if let Some(labels) = labels_by_id.remove(&issue.id) {
            issue.labels = labels;
        }
    }
    Ok(())
}

fn empty_ready_message(storage: &SqliteStorage) -> Result<&'static str> {
    let has_non_closed_issues = storage.has_active_issues()?;
    Ok(if has_non_closed_issues {
        "✨ No ready issues — all remaining work is blocked, deferred, or in progress"
    } else {
        "✨ All work complete — no issues to work on"
    })
}

fn get_ready_issues_for_output(
    storage: &SqliteStorage,
    filters: &ReadyFilters,
    sort_policy: ReadySortPolicy,
    output_format: OutputFormat,
) -> Result<Vec<crate::model::Issue>> {
    match output_format {
        OutputFormat::Text | OutputFormat::Csv => {
            storage.get_ready_summary_issues_for_command_output(filters, sort_policy)
        }
        OutputFormat::Json | OutputFormat::Toon => {
            storage.get_ready_issues_for_command_output(filters, sort_policy)
        }
    }
}

fn format_ready_line(
    index: usize,
    issue: &crate::model::Issue,
    use_color: bool,
    max_width: Option<usize>,
    wrap: bool,
) -> String {
    // Match bd format: {index}. [● P{n}] [{type}] {id}: {title}
    let priority_badge_plain = format!("[● {}]", crate::format::format_priority(&issue.priority));
    let type_badge_plain = format_type_badge(&issue.issue_type);
    let prefix_plain = format!(
        "{index}. {priority_badge_plain} {type_badge_plain} {}: ",
        issue.id
    );
    let title = if wrap {
        crate::format::sanitize_terminal_inline(&issue.title).into_owned()
    } else {
        max_width.map_or_else(
            || crate::format::sanitize_terminal_inline(&issue.title).into_owned(),
            |width| {
                let max_title = width.saturating_sub(UnicodeWidthStr::width(prefix_plain.as_str()));
                truncate_title(&issue.title, max_title)
            },
        )
    };

    let priority_badge = format_priority_badge(&issue.priority, use_color);
    let type_badge = crate::format::format_type_badge_colored(&issue.issue_type, use_color);
    format!(
        "{index}. {priority_badge} {type_badge} {}: {title}",
        issue.id
    )
}

/// Parse type filter strings to `IssueType` enums.
fn parse_types(types: &[String]) -> Result<Option<Vec<IssueType>>> {
    if types.is_empty() {
        return Ok(None);
    }

    let parsed = types
        .iter()
        .map(|t| t.parse())
        .collect::<Result<Vec<IssueType>>>()?;

    Ok(Some(parsed))
}

/// Parse priority filter strings to Priority values.
fn parse_priorities(priorities: &[String]) -> Result<Option<Vec<Priority>>> {
    if priorities.is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::with_capacity(priorities.len());
    for p in priorities {
        parsed.push(Priority::from_str(p)?);
    }

    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    #[test]
    fn test_parse_types() {
        init_logging();
        info!("test_parse_types: starting");
        let t = parse_types(&["bug".to_string(), "feature".to_string()])
            .expect("parse types")
            .expect("types");
        assert_eq!(t.len(), 2);
        info!("test_parse_types: assertions passed");
    }

    #[test]
    fn test_parse_priorities() {
        init_logging();
        info!("test_parse_priorities: starting");
        let p = parse_priorities(&["0".to_string(), "P1".to_string(), "2".to_string()])
            .expect("parse priorities")
            .unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].0, 0);
        assert_eq!(p[1].0, 1);
        assert_eq!(p[2].0, 2);
        info!("test_parse_priorities: assertions passed");
    }
}
