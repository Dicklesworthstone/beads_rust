//! List command implementation.
//!
//! Primary discovery interface with classic filter semantics and
//! paginated `ListPage` JSON output. Supports text, JSON, and CSV formats.

use crate::cli::{
    DEFAULT_LIST_LIMIT, DEFAULT_LIST_OFFSET, ListArgs, OutputFormat,
    resolve_output_format_with_outer_mode,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::csv;
use crate::format::{
    IssueWithCounts, ListPage, TextFormatOptions, format_issue_line_with, format_issue_long_with,
    format_issue_pretty_with, terminal_width,
};
use crate::model::{IssueType, Priority, Status};
use crate::output::{IssueTable, IssueTableColumns, JsonArrayPageMeta, OutputContext, OutputMode};
use crate::storage::ListFilters;
use crate::storage::sqlite::ListRelationMetadata;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;

// Large default-visible structured pages are faster through the existing full
// scan/relation path; smaller pages keep the medium-page relation queries.
const LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD: usize = 96;

/// Execute the list command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &ListArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    // Open storage (--db flag allows working from any directory)
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_inner(args, cli, outer_ctx, &storage_ctx)
}

/// Execute list using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if the list query or rendering fails.
pub fn execute_with_storage(
    args: &ListArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_inner(args, cli, outer_ctx, storage_ctx)
}

#[allow(clippy::too_many_lines)]
fn execute_inner(
    args: &ListArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let storage = &storage_ctx.storage;

    // Build filter from args
    let mut filters = build_filters(args)?;
    let client_filters = needs_client_filters(args);

    // Determine output format early so we know whether to run a count query.
    let output_format = resolve_output_format_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let is_json_output = matches!(output_format, OutputFormat::Json | OutputFormat::Toon);

    // The effective limit and offset from the user's request.
    let user_limit = args.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let user_offset = args.offset.unwrap_or(DEFAULT_LIST_OFFSET);
    let use_full_default_visible_structured_scan = should_use_full_default_visible_structured_scan(
        args,
        client_filters,
        is_json_output,
        user_limit,
        user_offset,
    );
    if use_full_default_visible_structured_scan {
        filters.limit = Some(0);
        filters.offset = Some(0);
    }

    // For paginated structured SQL-path queries, run a COUNT(*) query using the
    // same filters (without LIMIT/OFFSET) so we can include pagination metadata.
    // Unlimited output already materializes the full matching set, so its exact
    // total is the issue vector length after the list query.
    // For client-filter path, the total count is determined after filtering in Rust.
    let needs_sql_total = is_json_output
        && !client_filters
        && !use_full_default_visible_structured_scan
        && (user_limit != 0 || user_offset != 0);
    let sql_total: Option<usize> = if needs_sql_total {
        Some(storage.count_issues_with_filters(&filters)?)
    } else {
        None
    };

    // Extract user limit for both paths so we can detect truncation.
    let limit_for_truncation = if client_filters {
        // Remove LIMIT and OFFSET from the SQL query — the client-filter path
        // must fetch all issues, apply Rust-side filters, and then apply
        // offset + limit in Rust to get correct pagination.
        filters.limit.take();
        filters.offset.take();
        Some(user_limit)
    } else {
        // Bump SQL limit by 1 to detect whether results were truncated (text output).
        // For JSON output, we already have the exact total from the count query.
        let ul = if use_full_default_visible_structured_scan {
            Some(user_limit)
        } else {
            filters.limit
        };
        if !is_json_output
            && let Some(lim) = filters.limit
            && lim > 0
        {
            filters.limit = Some(lim + 1);
        }
        ul
    };

    // Validate sort key before query
    validate_sort_key(args.sort.as_deref())?;

    let use_projected_text_rows = matches!(output_format, OutputFormat::Text)
        && !args.long
        && !args.pretty
        && !client_filters;

    // Query issues
    let mut issues = if use_projected_text_rows {
        storage.list_text_issues_for_command_output(&filters)?
    } else {
        storage.list_issues(&filters)?
    };
    if client_filters {
        issues = apply_client_filters(issues, args)?;
    }

    // For JSON output, determine the total matching count.
    // For client-filter path, we now know the exact total before truncation.
    let json_total: usize = if is_json_output {
        sql_total.unwrap_or(issues.len())
    } else {
        0 // unused for text/csv output
    };

    // For client-filter path, apply offset here (after filtering) since it
    // was removed from the SQL query.  SQL-path offset is already applied by
    // the database engine.
    if client_filters && user_offset > 0 {
        if user_offset >= issues.len() {
            issues.clear();
        } else {
            issues = issues.split_off(user_offset);
        }
    }

    // Detect and apply truncation.
    // For client-filter path we know the exact pre-truncation count.
    // For SQL path we only know "more than limit" (we fetched limit+1 for text output).
    let total_before = issues.len();
    let truncated = if let Some(limit) = limit_for_truncation
        && limit > 0
        && issues.len() > limit
    {
        issues.truncate(limit);
        true
    } else {
        false
    };

    let quiet = cli.quiet.unwrap_or(false);
    let early_ctx = OutputContext::from_output_format(output_format, quiet, true);

    // Warn on stderr when results were truncated (skip for structured output)
    if truncated && !quiet && !matches!(output_format, OutputFormat::Json | OutputFormat::Toon) {
        if client_filters {
            // Exact total known from client-side filtering
            eprintln!(
                "[note] Showing {} of {} issues. Use --limit 0 for all results.",
                issues.len(),
                total_before,
            );
        } else {
            // SQL-side truncation: we only know there are more
            eprintln!(
                "[note] Output truncated to {} issues. Use --limit 0 for all results.",
                issues.len(),
            );
        }
    }
    if matches!(early_ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    // Output
    match output_format {
        OutputFormat::Json | OutputFormat::Toon => {
            let ctx = OutputContext::from_output_format(output_format, quiet, true);
            let use_full_relation_scan = use_full_default_visible_structured_scan
                || should_use_full_relation_scan(args, client_filters, user_limit, user_offset);

            let has_more = if user_limit == 0 {
                false
            } else {
                json_total > user_offset.saturating_add(user_limit)
            };

            let page_meta = JsonArrayPageMeta {
                total: json_total,
                limit: user_limit,
                offset: user_offset,
                has_more,
            };

            if matches!(output_format, OutputFormat::Toon) {
                let issues_with_counts =
                    collect_issues_with_counts(storage, issues, use_full_relation_scan)?;
                let page = ListPage {
                    issues: issues_with_counts,
                    total: page_meta.total,
                    limit: page_meta.limit,
                    offset: page_meta.offset,
                    has_more: page_meta.has_more,
                };
                if !ctx.toon_list_page_with_stats(&page, args.stats) {
                    ctx.toon_with_stats(&page, args.stats);
                }
            } else {
                stream_issues_with_counts(
                    &ctx,
                    storage,
                    issues,
                    use_full_relation_scan,
                    page_meta,
                )?;
            }
        }
        OutputFormat::Csv => {
            let fields = csv::parse_fields(args.fields.as_deref());
            let csv_output = csv::format_csv(&issues, &fields);
            print!("{csv_output}");
        }
        OutputFormat::Text => {
            let config_layer = storage_ctx.load_config(cli)?;
            let use_color = config::should_use_color(&config_layer);
            let max_width = if std::io::stdout().is_terminal() {
                Some(terminal_width())
            } else {
                None
            };
            let format_options = TextFormatOptions {
                use_color,
                max_width,
                wrap: args.wrap,
            };
            let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);
            if args.pretty {
                render_pretty_text_issues(&ctx, &issues, format_options, args.long);
            } else if matches!(ctx.mode(), OutputMode::Rich) {
                let columns = if args.long {
                    IssueTableColumns {
                        id: true,
                        priority: true,
                        status: true,
                        issue_type: true,
                        title: true,
                        assignee: true,
                        created: true,
                        updated: true,
                        ..Default::default()
                    }
                } else {
                    IssueTableColumns {
                        id: true,
                        priority: true,
                        status: true,
                        issue_type: true,
                        title: true,
                        ..Default::default()
                    }
                };
                let mut table = IssueTable::new(&issues, ctx.theme())
                    .columns(columns)
                    .title(format!("Issues ({})", issues.len()))
                    .wrap(args.wrap);
                if args.wrap {
                    table = table.width(Some(ctx.width()));
                }
                let table = table.build();
                ctx.render(&table);
            } else if args.long {
                render_long_text_issues(&ctx, &issues, format_options);
            } else {
                // Note: bd outputs nothing when no issues found, matching that for conformance
                for issue in &issues {
                    let line = format_issue_line_with(issue, format_options);
                    println!("{line}");
                }
            }
        }
    }

    Ok(())
}

fn collect_issues_with_counts(
    storage: &crate::storage::SqliteStorage,
    issues: Vec<crate::model::Issue>,
    use_full_relation_scan: bool,
) -> Result<Vec<IssueWithCounts>> {
    if use_full_relation_scan {
        let mut relation_metadata = storage.get_all_list_relation_metadata()?;
        Ok(issues
            .into_iter()
            .map(|issue| issue_with_full_relation_metadata(issue, &mut relation_metadata))
            .collect())
    } else {
        let (mut labels_map, dependency_counts, dependent_counts) =
            load_relation_metadata_for_issues(storage, &issues)?;
        Ok(issues
            .into_iter()
            .map(|issue| {
                issue_with_batched_relation_metadata(
                    issue,
                    &mut labels_map,
                    &dependency_counts,
                    &dependent_counts,
                )
            })
            .collect())
    }
}

fn stream_issues_with_counts(
    ctx: &OutputContext,
    storage: &crate::storage::SqliteStorage,
    issues: Vec<crate::model::Issue>,
    use_full_relation_scan: bool,
    page_meta: JsonArrayPageMeta,
) -> Result<()> {
    if use_full_relation_scan {
        let mut relation_metadata = storage.get_all_list_relation_metadata()?;
        ctx.json_array_page(
            "issues",
            issues
                .into_iter()
                .map(|issue| issue_with_full_relation_metadata(issue, &mut relation_metadata)),
            page_meta,
        );
    } else {
        let (mut labels_map, dependency_counts, dependent_counts) =
            load_relation_metadata_for_issues(storage, &issues)?;
        ctx.json_array_page(
            "issues",
            issues.into_iter().map(|issue| {
                issue_with_batched_relation_metadata(
                    issue,
                    &mut labels_map,
                    &dependency_counts,
                    &dependent_counts,
                )
            }),
            page_meta,
        );
    }
    Ok(())
}

type BatchedRelationMetadata = (
    HashMap<String, Vec<String>>,
    HashMap<String, usize>,
    HashMap<String, usize>,
);

fn load_relation_metadata_for_issues(
    storage: &crate::storage::SqliteStorage,
    issues: &[crate::model::Issue],
) -> Result<BatchedRelationMetadata> {
    let issue_ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let labels_map = storage.get_labels_for_issues(&issue_ids)?;
    let (dependency_counts, dependent_counts) =
        storage.count_relation_counts_for_issues(&issue_ids)?;
    Ok((labels_map, dependency_counts, dependent_counts))
}

fn issue_with_full_relation_metadata(
    mut issue: crate::model::Issue,
    relation_metadata: &mut HashMap<String, ListRelationMetadata>,
) -> IssueWithCounts {
    let metadata = relation_metadata.remove(&issue.id).unwrap_or_default();
    issue.labels = metadata.labels;

    IssueWithCounts {
        issue,
        dependency_count: metadata.dependency_count,
        dependent_count: metadata.dependent_count,
    }
}

fn issue_with_batched_relation_metadata(
    mut issue: crate::model::Issue,
    labels_map: &mut HashMap<String, Vec<String>>,
    dependency_counts: &HashMap<String, usize>,
    dependent_counts: &HashMap<String, usize>,
) -> IssueWithCounts {
    if let Some(labels) = labels_map.remove(&issue.id) {
        issue.labels = labels;
    }

    let dependency_count = *dependency_counts.get(&issue.id).unwrap_or(&0);
    let dependent_count = *dependent_counts.get(&issue.id).unwrap_or(&0);

    IssueWithCounts {
        issue,
        dependency_count,
        dependent_count,
    }
}

/// Expand `--priority`/`-p` raw values into a flat list of Priority values.
///
/// Each raw value may be a single priority (`0`, `P0`), a comma-separated
/// list (`0,1,2`), an inclusive range (`0-2`, `P0-P3`), or a mix
/// (`0,P2-P3`). Repeated `-p` flags are concatenated. Duplicates are removed
/// while preserving first-occurrence order. Empty entries (e.g. trailing
/// commas) are silently skipped. Each non-range piece must parse as a valid
/// Priority via `FromStr`; otherwise the helper returns the same
/// `InvalidPriority` error the single-value path used to return.
fn expand_priority_specs(raw: &[String]) -> Result<Vec<Priority>> {
    let mut out: Vec<Priority> = Vec::new();
    let push_unique = |p: Priority, out: &mut Vec<Priority>| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for value in raw {
        for piece in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((lo_str, hi_str)) = piece.split_once('-')
                && !lo_str.is_empty()
                && !hi_str.is_empty()
            {
                let lo: Priority = lo_str.trim().parse()?;
                let hi: Priority = hi_str.trim().parse()?;
                let (a, b) = if lo.0 <= hi.0 {
                    (lo.0, hi.0)
                } else {
                    (hi.0, lo.0)
                };
                for n in a..=b {
                    push_unique(Priority(n), &mut out);
                }
            } else {
                let p: Priority = piece.parse()?;
                push_unique(p, &mut out);
            }
        }
    }
    Ok(out)
}

/// Convert CLI args to storage filter.
fn build_filters(args: &ListArgs) -> Result<ListFilters> {
    // Parse status strings to Status enums
    let statuses = if args.status.is_empty() {
        None
    } else {
        Some(
            args.status
                .iter()
                .map(|s| s.parse())
                .collect::<Result<Vec<Status>>>()?,
        )
    };

    // Parse type strings to IssueType enums
    let types = if args.type_.is_empty() {
        None
    } else {
        Some(
            args.type_
                .iter()
                .map(|t| t.parse())
                .collect::<Result<Vec<IssueType>>>()?,
        )
    };

    // Parse priority values. Each raw value may be a single priority, a
    // comma-separated list (`0,1`), an inclusive range (`0-2`), or a mix.
    // See `expand_priority_specs` above. Invalid values surface as errors
    // rather than being silently dropped.
    let priorities = if args.priority.is_empty() {
        None
    } else {
        let expanded = expand_priority_specs(&args.priority)?;
        if expanded.is_empty() {
            None
        } else {
            Some(expanded)
        }
    };

    let include_closed = args.all
        || statuses
            .as_ref()
            .is_some_and(|parsed| parsed.iter().any(Status::is_terminal));

    // Deferred issues are included by default (consistent with "open" status semantics).
    // They are only excluded when explicitly filtering by status that doesn't include deferred.
    let include_deferred = args.deferred
        || args.all
        || statuses.is_none()
        || statuses
            .as_ref()
            .is_some_and(|parsed| parsed.contains(&Status::Deferred));

    Ok(ListFilters {
        statuses,
        types,
        priorities,
        assignee: args.assignee.clone(),
        unassigned: args.unassigned,
        include_closed,
        include_deferred,
        include_templates: false,
        title_contains: args.title_contains.clone(),
        limit: Some(args.limit.unwrap_or(DEFAULT_LIST_LIMIT)),
        offset: Some(args.offset.unwrap_or(DEFAULT_LIST_OFFSET)),
        sort: args.sort.clone(),
        reverse: args.reverse,
        labels: if args.label.is_empty() {
            None
        } else {
            Some(args.label.clone())
        },
        labels_or: if args.label_any.is_empty() {
            None
        } else {
            Some(args.label_any.clone())
        },
        updated_before: None,
        updated_after: None,
    })
}

/// Validate `list`-compatible CLI filters without executing the query.
pub(crate) fn validate_list_args(args: &ListArgs) -> Result<()> {
    let _ = build_filters(args)?;
    validate_sort_key(args.sort.as_deref())?;
    validate_priority_bounds(args.priority_min, args.priority_max)?;
    Ok(())
}

fn needs_client_filters(args: &ListArgs) -> bool {
    !args.id.is_empty()
        || args.priority_min.is_some()
        || args.priority_max.is_some()
        || args.desc_contains.is_some()
        || args.notes_contains.is_some()
        || args.deferred
        || args.overdue
}

fn should_use_full_relation_scan(
    args: &ListArgs,
    client_filters: bool,
    user_limit: usize,
    user_offset: usize,
) -> bool {
    !client_filters
        && user_limit == 0
        && user_offset == 0
        && args.status.is_empty()
        && args.type_.is_empty()
        && args.priority.is_empty()
        && args.assignee.is_none()
        && !args.unassigned
        && args.title_contains.is_none()
        && args.label.is_empty()
        && args.label_any.is_empty()
}

fn should_use_full_default_visible_structured_scan(
    args: &ListArgs,
    client_filters: bool,
    is_structured_output: bool,
    user_limit: usize,
    user_offset: usize,
) -> bool {
    is_structured_output
        && !client_filters
        && user_offset == 0
        && user_limit >= LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD
        && !args.all
        && args.status.is_empty()
        && args.type_.is_empty()
        && args.priority.is_empty()
        && args.assignee.is_none()
        && !args.unassigned
        && args.title_contains.is_none()
        && args.label.is_empty()
        && args.label_any.is_empty()
        && args.sort.is_none()
        && !args.reverse
}

fn apply_client_filters(
    issues: Vec<crate::model::Issue>,
    args: &ListArgs,
) -> Result<Vec<crate::model::Issue>> {
    let id_filter: Option<HashSet<&str>> = if args.id.is_empty() {
        None
    } else {
        Some(args.id.iter().map(String::as_str).collect())
    };

    let mut filtered = Vec::new();
    let now = Utc::now();
    let min_priority = args.priority_min.map(i32::from);
    let max_priority = args.priority_max.map(i32::from);
    let desc_needle = args.desc_contains.as_deref().map(str::to_lowercase);
    let notes_needle = args.notes_contains.as_deref().map(str::to_lowercase);
    // Deferred issues are included by default when no status filter is specified,
    // except `--overdue` keeps deferred work hidden unless requested.
    let include_deferred = args.deferred
        || args.all
        || (!args.overdue && args.status.is_empty())
        || args
            .status
            .iter()
            .any(|status| status.eq_ignore_ascii_case("deferred"));

    validate_priority_bounds(args.priority_min, args.priority_max)?;

    for issue in issues {
        if let Some(ids) = &id_filter
            && !ids.contains(issue.id.as_str())
        {
            continue;
        }

        if let Some(min) = min_priority
            && issue.priority.0 < min
        {
            continue;
        }
        if let Some(max) = max_priority
            && issue.priority.0 > max
        {
            continue;
        }

        if let Some(ref needle) = desc_needle {
            let haystack = issue.description.as_deref().unwrap_or("").to_lowercase();
            if !haystack.contains(needle) {
                continue;
            }
        }

        if let Some(ref needle) = notes_needle {
            let haystack = issue.notes.as_deref().unwrap_or("").to_lowercase();
            if !haystack.contains(needle) {
                continue;
            }
        }

        if !include_deferred && matches!(issue.status, Status::Deferred) {
            continue;
        }

        if args.overdue {
            let overdue = issue.due_at.is_some_and(|due| due < now) && !issue.status.is_terminal();
            if !overdue {
                continue;
            }
        }

        filtered.push(issue);
    }

    Ok(filtered)
}

fn render_long_text_issues(
    ctx: &OutputContext,
    issues: &[crate::model::Issue],
    format_options: TextFormatOptions,
) {
    for (index, issue) in issues.iter().enumerate() {
        ctx.print_line(&format_issue_long_with(issue, format_options));
        if index + 1 != issues.len() {
            ctx.print_line("");
        }
    }
}

fn render_pretty_text_issues(
    ctx: &OutputContext,
    issues: &[crate::model::Issue],
    format_options: TextFormatOptions,
    include_extended: bool,
) {
    for (index, issue) in issues.iter().enumerate() {
        ctx.print_line(&format_issue_pretty_with(
            issue,
            format_options,
            include_extended,
        ));
        if index + 1 != issues.len() {
            ctx.print_line("");
        }
    }
}

fn validate_sort_key(sort: Option<&str>) -> Result<()> {
    let Some(sort_key) = sort else {
        return Ok(());
    };

    match sort_key {
        "priority" | "created_at" | "updated_at" | "title" | "created" | "updated" => Ok(()),
        _ => Err(BeadsError::Validation {
            field: "sort".to_string(),
            reason: format!("invalid sort field '{sort_key}'"),
        }),
    }
}

fn validate_priority_bounds(priority_min: Option<u8>, priority_max: Option<u8>) -> Result<()> {
    if let Some(min) = priority_min.map(i32::from)
        && !(0..=4).contains(&min)
    {
        return Err(BeadsError::InvalidPriority {
            priority: min.to_string(),
        });
    }

    if let Some(max) = priority_max.map(i32::from)
        && !(0..=4).contains(&max)
    {
        return Err(BeadsError::InvalidPriority {
            priority: max.to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
    use crate::model::Issue;
    use chrono::Duration;
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    #[test]
    fn test_build_filters_includes_closed_for_terminal_status() {
        init_logging();
        info!("test_build_filters_includes_closed_for_terminal_status: starting");
        let args = cli::ListArgs {
            status: vec!["closed".to_string()],
            ..Default::default()
        };

        let filters = build_filters(&args).expect("build filters");
        assert!(filters.include_closed);
        assert!(
            filters
                .statuses
                .as_ref()
                .expect("statuses")
                .contains(&Status::Closed)
        );
        info!("test_build_filters_includes_closed_for_terminal_status: assertions passed");
    }

    #[test]
    fn test_build_filters_parses_priorities() {
        init_logging();
        info!("test_build_filters_parses_priorities: starting");
        let args = cli::ListArgs {
            priority: vec!["0".to_string(), "2".to_string()],
            ..Default::default()
        };

        let filters = build_filters(&args).expect("build filters");
        let priorities = filters.priorities.expect("priorities");
        let values: Vec<i32> = priorities.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 2]);
        info!("test_build_filters_parses_priorities: assertions passed");
    }

    #[test]
    fn test_expand_priority_specs_comma_list() {
        let out = expand_priority_specs(&["0,1,2".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1, 2]);
    }

    #[test]
    fn test_expand_priority_specs_inclusive_range() {
        let out = expand_priority_specs(&["0-2".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1, 2]);
    }

    #[test]
    fn test_expand_priority_specs_range_with_p_prefix() {
        let out = expand_priority_specs(&["P1-P3".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn test_expand_priority_specs_reversed_range_normalizes() {
        let out = expand_priority_specs(&["3-1".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn test_expand_priority_specs_mixed_repeated_and_csv() {
        let out =
            expand_priority_specs(&["0,P2".to_string(), "3".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 2, 3]);
    }

    #[test]
    fn test_expand_priority_specs_dedup_preserves_first_occurrence() {
        let out =
            expand_priority_specs(&["0,1".to_string(), "1,2,0".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1, 2]);
    }

    #[test]
    fn test_expand_priority_specs_skips_empty_pieces() {
        // Trailing commas and stray whitespace must not produce a value.
        let out = expand_priority_specs(&[" 0, ,1, ".to_string()]).expect("expand");
        let values: Vec<i32> = out.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1]);
    }

    #[test]
    fn test_expand_priority_specs_invalid_in_list_errors() {
        // 9 is out of range; the whole call must error rather than silently
        // returning the valid prefix.
        let err = expand_priority_specs(&["0,9".to_string()]).expect_err("should error");
        assert!(
            err.to_string().contains("Priority"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_build_filters_accepts_comma_list_priority() {
        // Regression: `br list --priority 0,1` used to error with
        // "Priority must be 0-4, got: 0,1". Now it expands to [P0, P1].
        let args = cli::ListArgs {
            priority: vec!["0,1".to_string()],
            ..Default::default()
        };
        let filters = build_filters(&args).expect("build filters");
        let priorities = filters.priorities.expect("priorities");
        let values: Vec<i32> = priorities.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1]);
    }

    #[test]
    fn test_build_filters_accepts_range_priority() {
        // Regression: `br list -p 0-1` is documented in the repo CLAUDE.md
        // as working; this makes the doc claim true.
        let args = cli::ListArgs {
            priority: vec!["0-1".to_string()],
            ..Default::default()
        };
        let filters = build_filters(&args).expect("build filters");
        let priorities = filters.priorities.expect("priorities");
        let values: Vec<i32> = priorities.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 1]);
    }

    #[test]
    fn test_build_filters_applies_list_defaults_when_cli_omits_pagination() {
        init_logging();
        let filters = build_filters(&ListArgs::default()).expect("build filters");

        assert_eq!(filters.limit, Some(DEFAULT_LIST_LIMIT));
        assert_eq!(filters.offset, Some(DEFAULT_LIST_OFFSET));
    }

    #[test]
    fn test_needs_client_filters_detects_fields() {
        init_logging();
        info!("test_needs_client_filters_detects_fields: starting");
        let args = ListArgs::default();
        assert!(!needs_client_filters(&args));

        let args = cli::ListArgs {
            label: vec!["backend".to_string()],
            ..Default::default()
        };
        assert!(!needs_client_filters(&args));

        let args = cli::ListArgs {
            desc_contains: Some("needle".to_string()),
            ..Default::default()
        };
        assert!(needs_client_filters(&args));

        let args = cli::ListArgs {
            label: vec!["backend".to_string()],
            desc_contains: Some("needle".to_string()),
            ..Default::default()
        };
        assert!(needs_client_filters(&args));
        info!("test_needs_client_filters_detects_fields: assertions passed");
    }

    fn issue_with_id(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            ..Issue::default()
        }
    }

    #[test]
    fn test_apply_client_filters_honors_id_priority_and_text_filters() {
        init_logging();
        let mut matching = issue_with_id("bd-2", "matching issue");
        matching.priority = Priority(2);
        matching.description = Some("Contains a unique NEEDLE".to_string());
        matching.notes = Some("Tracker note with token".to_string());

        let mut wrong_id = issue_with_id("bd-1", "wrong id");
        wrong_id.priority = Priority(2);
        wrong_id.description = Some("Contains a unique needle".to_string());
        wrong_id.notes = Some("Tracker note with token".to_string());

        let mut wrong_priority = issue_with_id("bd-3", "wrong priority");
        wrong_priority.priority = Priority(4);
        wrong_priority.description = Some("Contains a unique needle".to_string());
        wrong_priority.notes = Some("Tracker note with token".to_string());

        let args = ListArgs {
            id: vec!["bd-2".to_string()],
            priority_min: Some(2),
            priority_max: Some(2),
            desc_contains: Some("needle".to_string()),
            notes_contains: Some("token".to_string()),
            ..Default::default()
        };

        let filtered = apply_client_filters(vec![wrong_id, wrong_priority, matching], &args)
            .expect("apply client filters");
        let ids: Vec<_> = filtered.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-2"]);
    }

    #[test]
    fn test_apply_client_filters_excludes_deferred_from_overdue_unless_requested() {
        init_logging();
        let now = Utc::now();

        let mut overdue_open = issue_with_id("bd-1", "overdue open");
        overdue_open.due_at = Some(now - Duration::days(1));

        let mut overdue_deferred = issue_with_id("bd-2", "overdue deferred");
        overdue_deferred.status = Status::Deferred;
        overdue_deferred.due_at = Some(now - Duration::days(1));

        let mut future_open = issue_with_id("bd-3", "future open");
        future_open.due_at = Some(now + Duration::days(1));

        let mut overdue_closed = issue_with_id("bd-4", "overdue closed");
        overdue_closed.status = Status::Closed;
        overdue_closed.due_at = Some(now - Duration::days(1));

        let overdue_only = apply_client_filters(
            vec![
                overdue_open.clone(),
                overdue_deferred.clone(),
                future_open,
                overdue_closed,
            ],
            &ListArgs {
                overdue: true,
                ..Default::default()
            },
        )
        .expect("overdue filter");
        let overdue_only_ids: Vec<_> = overdue_only.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(overdue_only_ids, vec!["bd-1"]);

        let overdue_with_deferred = apply_client_filters(
            vec![overdue_open.clone(), overdue_deferred.clone()],
            &ListArgs {
                overdue: true,
                deferred: true,
                ..Default::default()
            },
        )
        .expect("overdue with deferred filter");
        let overdue_with_deferred_ids: Vec<_> = overdue_with_deferred
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(overdue_with_deferred_ids, vec!["bd-1", "bd-2"]);

        let overdue_with_all = apply_client_filters(
            vec![overdue_open, overdue_deferred],
            &ListArgs {
                overdue: true,
                all: true,
                ..Default::default()
            },
        )
        .expect("overdue with all filter");
        let overdue_with_all_ids: Vec<_> = overdue_with_all
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(overdue_with_all_ids, vec!["bd-1", "bd-2"]);
    }

    #[test]
    fn test_validate_list_args_rejects_invalid_sort() {
        init_logging();
        let err = validate_list_args(&ListArgs {
            sort: Some("nonsense".to_string()),
            ..Default::default()
        })
        .expect_err("invalid sort should fail");

        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "sort"));
    }

    #[test]
    fn test_full_relation_scan_covers_unbounded_default_json_list() {
        init_logging();
        assert!(should_use_full_relation_scan(
            &ListArgs {
                limit: Some(0),
                ..Default::default()
            },
            false,
            0,
            0,
        ));

        assert!(!should_use_full_relation_scan(
            &ListArgs {
                limit: Some(50),
                ..Default::default()
            },
            false,
            50,
            0,
        ));

        assert!(!should_use_full_relation_scan(
            &ListArgs {
                limit: Some(0),
                label: vec!["backend".to_string()],
                ..Default::default()
            },
            false,
            0,
            0,
        ));
    }

    #[test]
    fn test_large_structured_pages_use_full_default_scan() {
        init_logging();
        assert!(should_use_full_default_visible_structured_scan(
            &ListArgs::default(),
            false,
            true,
            LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD,
            0,
        ));

        assert!(!should_use_full_default_visible_structured_scan(
            &ListArgs::default(),
            false,
            true,
            LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD - 1,
            0,
        ));

        assert!(!should_use_full_default_visible_structured_scan(
            &ListArgs {
                label: vec!["backend".to_string()],
                ..Default::default()
            },
            false,
            true,
            LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD,
            0,
        ));

        assert!(!should_use_full_default_visible_structured_scan(
            &ListArgs::default(),
            false,
            false,
            LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD,
            0,
        ));

        assert!(!should_use_full_default_visible_structured_scan(
            &ListArgs::default(),
            false,
            true,
            LARGE_STRUCTURED_LIST_FULL_SCAN_THRESHOLD,
            1,
        ));
    }

    #[test]
    fn test_validate_list_args_rejects_invalid_priority_bounds() {
        init_logging();
        let err = validate_list_args(&ListArgs {
            priority_min: Some(7),
            ..Default::default()
        })
        .expect_err("invalid priority should fail");

        assert!(matches!(
            err,
            BeadsError::InvalidPriority { ref priority } if priority == "7"
        ));
    }
}
