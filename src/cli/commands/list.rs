//! List command implementation.
//!
//! Primary discovery interface with classic filter semantics and
//! `IssueWithCounts` JSON output. Supports text, JSON, and CSV formats.

use crate::cli::{ListArgs, OutputFormat, resolve_output_format};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::csv;
use crate::format::{
    IssueWithCounts, TextFormatOptions, format_comment_marker, format_issue_line_with_comments,
    terminal_width,
};
use crate::model::{IssueType, Priority, Status};
use crate::output::{IssueTable, IssueTableColumns, OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::split_prefix_remainder;
use chrono::Utc;
use std::collections::HashSet;
use std::io::IsTerminal;

/// Resolve the prefix-scope for `bd list`.
///
/// There is no identity-based default scoping: agent IDs need not
/// correspond to issue prefixes, so silently narrowing `bd list`
/// output by the caller's identity would be confusing rather than
/// helpful. Default output includes every prefix; `--prefix` remains
/// available as an explicit filter.
fn resolve_list_prefix(args: &ListArgs) -> Option<String> {
    args.prefix
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

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
    let storage = &storage_ctx.storage;
    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let use_color = config::should_use_color(&config_layer);
    let max_width = if std::io::stdout().is_terminal() {
        Some(terminal_width())
    } else {
        None
    };
    let format_options = TextFormatOptions {
        use_color,
        max_width,
        wrap: !args.no_wrap,
    };

    // Build filter from args
    let mut filters = build_filters(args)?;
    let client_filters = needs_client_filters(args);
    let limit = if client_filters {
        filters.limit.take()
    } else {
        None
    };

    // Validate sort key before query
    validate_sort_key(args.sort.as_deref())?;

    // Query issues
    let issues = storage.list_issues(&filters)?;
    let mut issues = if client_filters {
        apply_client_filters(storage, issues, args)?
    } else {
        issues
    };

    // Explicit --prefix filter only. No identity-based default scoping
    // (see resolve_list_prefix doc comment) — default output shows every
    // prefix.
    if let Some(target) = resolve_list_prefix(args) {
        issues.retain(|issue| {
            split_prefix_remainder(&issue.id)
                .map(|(p, _)| p == target.as_str())
                .unwrap_or(false)
        });
    }

    if let Some(limit) = limit {
        if limit > 0 && issues.len() > limit {
            issues.truncate(limit);
        }
    }

    // Determine output format: --json flag overrides --format
    let output_format = resolve_output_format(args.format, outer_ctx.is_json(), false);
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    // Comment facts for the listed issues: a count and a recency, batched
    // in one query. Never bodies — a listing tells you history exists and
    // how fresh it is, and `bd comments <id>` shows it.
    let issue_ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
    let comment_stats = storage.comment_stats_for_issues(&issue_ids)?;

    // Output
    match output_format {
        OutputFormat::Json | OutputFormat::Toon => {
            // Fetch relations for all issues
            let mut labels_map = storage.get_labels_for_issues(&issue_ids)?;

            // Use batch counting
            let dependency_counts = storage.count_dependencies_for_issues(&issue_ids)?;
            let dependent_counts = storage.count_dependents_for_issues(&issue_ids)?;

            // Convert to IssueWithCounts
            let issues_with_counts: Vec<IssueWithCounts> = issues
                .into_iter()
                .map(|mut issue| {
                    if let Some(labels) = labels_map.remove(&issue.id) {
                        issue.labels = labels;
                    }

                    let dependency_count = *dependency_counts.get(&issue.id).unwrap_or(&0);
                    let dependent_count = *dependent_counts.get(&issue.id).unwrap_or(&0);
                    let stats = comment_stats.get(&issue.id).copied().unwrap_or_default();

                    IssueWithCounts {
                        issue,
                        dependency_count,
                        dependent_count,
                        comment_count: stats.count,
                        last_comment_at: stats.last_comment_at,
                        comment_match: false,
                    }
                })
                .collect();

            if matches!(output_format, OutputFormat::Toon) {
                ctx.toon_with_stats(&issues_with_counts, args.stats);
            } else {
                ctx.json_pretty(&issues_with_counts);
            }
        }
        OutputFormat::Csv => {
            let fields = csv::parse_fields(args.fields.as_deref());
            let csv_output = csv::format_csv(&issues, &fields);
            print!("{csv_output}");
        }
        OutputFormat::Text => {
            if matches!(ctx.mode(), OutputMode::Rich) {
                // `bd list`'s rich table is only ever built in
                // `OutputMode::Rich`, which (see `OutputContext::
                // detect_mode`/`from_output_format`) requires color to
                // be enabled and stdout to be a tty — `NO_COLOR` or a
                // non-tty always falls back to the plain-text line
                // path below instead. `color_available` documents that
                // invariant at the call site rather than assuming it
                // silently: if it's ever false here, the Type column
                // comes back (see below) so type information isn't
                // lost just because the ID color can't carry it.
                let color_available = matches!(ctx.mode(), OutputMode::Rich);
                let columns = IssueTableColumns {
                    id: true,
                    priority: true,
                    status: true,
                    // Default: the Type column is dropped and its
                    // signal is instead carried by coloring the ID
                    // cell (see `color_id_by_type` below) — but only
                    // when color is actually available; otherwise the
                    // column comes back so nothing is silently lost.
                    // `--long`: always show it textually regardless of
                    // color, which is also what keeps `--long` a
                    // meaningful distinct view now that Assignee is
                    // gone from it too and both views share the same
                    // Age column.
                    issue_type: args.long || !color_available,
                    title: true,
                    // Assignee is empty in practice; dropped from both
                    // the default and --long views to give the Title
                    // column the space back.
                    assignee: false,
                    // Creator (the agent identity that opened the bead)
                    // in --long only: the default view stays lean and
                    // title-focused, but --long is where you go to ask
                    // "who opened this?".
                    created_by: args.long,
                    // Combined compact age column (created/updated),
                    // same presentation as the plain-text line's age
                    // field — in both default and --long views, since
                    // %Y-%m-%d costs ~2x the width for less-legible
                    // information ("2024-03-01" vs "5d/2h").
                    age: true,
                    color_id_by_type: color_available,
                    ..Default::default()
                };
                let mut table = IssueTable::new(&issues, ctx.theme())
                    .columns(columns)
                    .title(format!("Issues ({})", issues.len()))
                    // Adds a narrow Cmts column, but only if some listed
                    // issue actually has comments — see `comment_markers`.
                    .comment_markers(comment_markers(&comment_stats))
                    .wrap(!args.no_wrap);
                if !args.no_wrap {
                    table = table.width(Some(ctx.width()));
                }
                let table = table.build();
                ctx.render(&table);
            } else {
                // Note: bd outputs nothing when no issues found, matching that for conformance
                for issue in &issues {
                    let marker = comment_stats
                        .get(&issue.id)
                        .and_then(|stats| format_comment_marker(stats.count, stats.last_comment_at));
                    let line =
                        format_issue_line_with_comments(issue, format_options, marker.as_deref());
                    println!("{line}");
                }
            }
        }
    }

    Ok(())
}

/// Build the per-issue compact comment markers for the rich table.
///
/// Only issues that have comments get an entry, which is also what decides
/// whether the column appears at all (an empty map leaves it off).
fn comment_markers(
    stats: &std::collections::HashMap<String, crate::storage::CommentStats>,
) -> std::collections::HashMap<String, String> {
    stats
        .iter()
        .filter_map(|(id, stats)| {
            format_comment_marker(stats.count, stats.last_comment_at)
                .map(|marker| (id.clone(), marker))
        })
        .collect()
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

    // Parse priority values (invalid values should error, not be silently dropped)
    let priorities = if args.priority.is_empty() {
        None
    } else {
        Some(
            args.priority
                .iter()
                .map(|p| p.parse())
                .collect::<Result<Vec<Priority>>>()?,
        )
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
        created_by: args.created_by.clone(),
        include_closed,
        include_deferred,
        include_templates: false,
        title_contains: args.title_contains.clone(),
        limit: args.limit,
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

fn needs_client_filters(args: &ListArgs) -> bool {
    !args.id.is_empty()
        || !args.label.is_empty()
        || !args.label_any.is_empty()
        || args.priority_min.is_some()
        || args.priority_max.is_some()
        || args.desc_contains.is_some()
        || args.notes_contains.is_some()
        || args.deferred
        || args.overdue
}

fn apply_client_filters(
    storage: &SqliteStorage,
    issues: Vec<crate::model::Issue>,
    args: &ListArgs,
) -> Result<Vec<crate::model::Issue>> {
    let id_filter: Option<HashSet<&str>> = if args.id.is_empty() {
        None
    } else {
        Some(args.id.iter().map(String::as_str).collect())
    };

    let label_filters = !args.label.is_empty() || !args.label_any.is_empty();

    // Pre-fetch labels if needed to avoid N+1
    let labels_map = if label_filters {
        let issue_ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
        storage.get_labels_for_issues(&issue_ids)?
    } else {
        std::collections::HashMap::new()
    };

    let mut filtered = Vec::new();
    let now = Utc::now();
    let min_priority = args.priority_min.map(i32::from);
    let max_priority = args.priority_max.map(i32::from);
    let desc_needle = args.desc_contains.as_deref().map(str::to_lowercase);
    let notes_needle = args.notes_contains.as_deref().map(str::to_lowercase);
    // Deferred issues are included by default when no status filter is specified
    let include_deferred = args.deferred
        || args.status.is_empty()
        || args
            .status
            .iter()
            .any(|status| status.eq_ignore_ascii_case("deferred"));

    if let Some(min) = min_priority {
        if !(0..=4).contains(&min) {
            return Err(BeadsError::InvalidPriority { priority: min });
        }
    }
    if let Some(max) = max_priority {
        if !(0..=4).contains(&max) {
            return Err(BeadsError::InvalidPriority { priority: max });
        }
    }

    for issue in issues {
        if let Some(ids) = &id_filter {
            if !ids.contains(issue.id.as_str()) {
                continue;
            }
        }

        if let Some(min) = min_priority {
            if issue.priority.0 < min {
                continue;
            }
        }
        if let Some(max) = max_priority {
            if issue.priority.0 > max {
                continue;
            }
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

        if label_filters {
            let default_labels = Vec::new();
            let labels = labels_map.get(&issue.id).unwrap_or(&default_labels);
            if !args.label.is_empty() && !args.label.iter().all(|label| labels.contains(label)) {
                continue;
            }
            if !args.label_any.is_empty()
                && !args.label_any.iter().any(|label| labels.contains(label))
            {
                continue;
            }
        }

        filtered.push(issue);
    }

    Ok(filtered)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
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
    fn test_resolve_list_prefix_defaults_to_none() {
        init_logging();
        info!("test_resolve_list_prefix_defaults_to_none: starting");
        let args = ListArgs::default();
        assert_eq!(resolve_list_prefix(&args), None);
        info!("test_resolve_list_prefix_defaults_to_none: assertions passed");
    }

    #[test]
    fn test_resolve_list_prefix_explicit_prefix_wins() {
        init_logging();
        let args = cli::ListArgs {
            prefix: Some("other1".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_list_prefix(&args), Some("other1".to_string()));
    }

    #[test]
    fn test_resolve_list_prefix_trims_whitespace() {
        init_logging();
        let args = cli::ListArgs {
            prefix: Some("  ".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_list_prefix(&args), None);
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
        assert!(needs_client_filters(&args));

        let args = cli::ListArgs {
            desc_contains: Some("needle".to_string()),
            ..Default::default()
        };
        assert!(needs_client_filters(&args));
        info!("test_needs_client_filters_detects_fields: assertions passed");
    }
}
