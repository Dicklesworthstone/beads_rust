//! Search command implementation.
//!
//! Classic bd-style substring search across title/description/id — plus
//! comment bodies (beads_rust#416) — with list-like filters.

use crate::cli::{
    DEFAULT_LIST_OFFSET, DEFAULT_SEARCH_LIMIT, ListArgs, OutputFormat, SearchArgs,
    resolve_output_format_with_outer_mode,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{
    IssueWithCounts, TextFormatOptions, csv, format_issue_line_with, terminal_width,
};
use crate::model::{Issue, IssueType, Priority, Status};
use crate::output::{IssueTable, IssueTableColumns, OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::Utc;
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::str::FromStr;

/// Execute the search command.
///
/// # Errors
///
/// Returns an error if the query is empty, the database cannot be opened,
/// or the query fails.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &SearchArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(BeadsError::Validation {
            field: "query".to_string(),
            reason: "search query cannot be empty".to_string(),
        });
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_with_storage_ctx(args, cli, outer_ctx, &storage_ctx)
}

/// Execute search using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if the query is empty or search execution fails.
#[allow(clippy::too_many_lines)]
pub fn execute_with_storage_ctx(
    args: &SearchArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let query = validate_query(args)?;
    let storage = &storage_ctx.storage;
    let output_format = resolve_output_format_with_outer_mode(
        args.filters.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let issues = collect_search_results_for_output(storage, query, &args.filters, output_format)?;
    render_search_results(
        storage,
        issues,
        query,
        &args.filters,
        output_format,
        cli,
        storage_ctx,
    )
}

fn validate_query(args: &SearchArgs) -> Result<&str> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(BeadsError::Validation {
            field: "query".to_string(),
            reason: "search query cannot be empty".to_string(),
        });
    }
    Ok(query)
}

#[cfg(test)]
fn collect_search_results(
    storage: &SqliteStorage,
    query: &str,
    list_args: &ListArgs,
) -> Result<Vec<Issue>> {
    collect_search_results_with_projection(storage, query, list_args, false)
}

fn collect_search_results_for_output(
    storage: &SqliteStorage,
    query: &str,
    list_args: &ListArgs,
    output_format: OutputFormat,
) -> Result<Vec<Issue>> {
    collect_search_results_with_projection(
        storage,
        query,
        list_args,
        matches!(output_format, OutputFormat::Text),
    )
}

fn collect_search_results_with_projection(
    storage: &SqliteStorage,
    query: &str,
    list_args: &ListArgs,
    use_text_projection: bool,
) -> Result<Vec<Issue>> {
    let mut filters = build_filters(list_args)?;
    let client_filters = needs_client_filters(list_args);
    let needs_post_query_ordering = requires_post_query_ordering(list_args, client_filters);
    let (offset, limit) = if needs_post_query_ordering {
        (filters.offset.take(), filters.limit.take())
    } else {
        (None, None)
    };
    if needs_post_query_ordering {
        filters.sort = None;
        filters.reverse = false;
    }

    let issues = if use_text_projection && !client_filters {
        storage.search_issues_for_command_output(query, &filters)?
    } else {
        storage.search_issues(query, &filters)?
    };
    let mut issues = if client_filters {
        apply_client_filters(issues, list_args)?
    } else {
        issues
    };

    if needs_post_query_ordering {
        apply_issue_sort(&mut issues, list_args.sort.as_deref(), list_args.reverse)?;
        if let Some(offset) = offset
            && offset > 0
        {
            if offset >= issues.len() {
                issues.clear();
            } else {
                issues = issues.split_off(offset);
            }
        }
        if let Some(limit) = limit
            && limit > 0
            && issues.len() > limit
        {
            issues.truncate(limit);
        }
    }

    Ok(issues)
}

#[allow(clippy::too_many_lines)]
fn render_search_results(
    storage: &SqliteStorage,
    issues: Vec<Issue>,
    query: &str,
    list_args: &ListArgs,
    output_format: OutputFormat,
    cli: &config::CliOverrides,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let quiet = cli.quiet.unwrap_or(false);
    let early_ctx = OutputContext::from_output_format(output_format, quiet, true);
    if matches!(early_ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    match output_format {
        OutputFormat::Json => {
            let mut relation_metadata = load_search_relation_metadata(storage, &issues)?;
            early_ctx.json_array(
                issues
                    .into_iter()
                    .map(|issue| issue_with_counts(issue, &mut relation_metadata)),
            );
            return Ok(());
        }
        OutputFormat::Toon => {
            let issues_with_counts = attach_counts(storage, issues)?;
            if early_ctx.toon_issue_counts_array_with_stats(&issues_with_counts, list_args.stats) {
                return Ok(());
            }
            early_ctx.toon_with_stats(&issues_with_counts, list_args.stats);
            return Ok(());
        }
        OutputFormat::Csv => {
            let fields = csv::parse_fields(list_args.fields.as_deref());
            let csv_output = csv::format_csv(&issues, &fields);
            print!("{csv_output}");
            return Ok(());
        }
        OutputFormat::Text => {}
    }

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
        wrap: list_args.wrap,
    };
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);

    if matches!(ctx.mode(), OutputMode::Rich) {
        let context_snippets = build_context_snippets(&issues, query);
        let show_context = !context_snippets.is_empty();
        let columns = IssueTableColumns {
            id: true,
            priority: true,
            status: true,
            issue_type: true,
            title: true,
            assignee: true,
            context: show_context,
            ..Default::default()
        };
        let mut table = IssueTable::new(&issues, ctx.theme())
            .columns(columns)
            .title(format!(
                "Search: \"{}\" - {} result{}",
                query,
                issues.len(),
                if issues.len() == 1 { "" } else { "s" }
            ))
            .highlight_query(query)
            .wrap(list_args.wrap);
        if list_args.wrap {
            table = table.width(Some(ctx.width()));
        }
        if show_context {
            table = table.context_snippets(context_snippets);
        }
        ctx.render(&table.build());
        return Ok(());
    }

    ctx.info(&format!(
        "Found {} issue(s) matching '{}'",
        issues.len(),
        query
    ));
    for issue in &issues {
        let line = format_issue_line_with(issue, format_options);
        ctx.print_line(&line);
    }

    Ok(())
}

#[derive(Default)]
struct SearchRelationMetadata {
    labels_by_id: HashMap<String, Vec<String>>,
    dependency_counts: HashMap<String, usize>,
    dependent_counts: HashMap<String, usize>,
}

fn load_search_relation_metadata(
    storage: &SqliteStorage,
    issues: &[Issue],
) -> Result<SearchRelationMetadata> {
    if issues.is_empty() {
        return Ok(SearchRelationMetadata::default());
    }

    let issue_ids: Vec<String> = issues.iter().map(|issue| issue.id.clone()).collect();
    let labels_by_id = storage.get_labels_for_issues(&issue_ids)?;
    let (dependency_counts, dependent_counts) =
        storage.count_relation_counts_for_issues(&issue_ids)?;

    Ok(SearchRelationMetadata {
        labels_by_id,
        dependency_counts,
        dependent_counts,
    })
}

fn issue_with_counts(
    mut issue: Issue,
    relation_metadata: &mut SearchRelationMetadata,
) -> IssueWithCounts {
    let dependency_count = *relation_metadata
        .dependency_counts
        .get(&issue.id)
        .unwrap_or(&0);
    let dependent_count = *relation_metadata
        .dependent_counts
        .get(&issue.id)
        .unwrap_or(&0);
    if let Some(labels) = relation_metadata.labels_by_id.remove(&issue.id) {
        issue.labels = labels;
    }
    IssueWithCounts {
        issue,
        dependency_count,
        dependent_count,
    }
}

fn attach_counts(
    storage: &SqliteStorage,
    issues: Vec<crate::model::Issue>,
) -> Result<Vec<IssueWithCounts>> {
    let mut relation_metadata = load_search_relation_metadata(storage, &issues)?;

    Ok(issues
        .into_iter()
        .map(|issue| issue_with_counts(issue, &mut relation_metadata))
        .collect())
}

fn build_context_snippets(issues: &[crate::model::Issue], query: &str) -> HashMap<String, String> {
    let Some(regex) = build_highlight_regex(query) else {
        return HashMap::new();
    };

    let mut snippets = HashMap::new();
    for issue in issues {
        if let Some(description) = issue.description.as_deref()
            && let Some(mat) = regex.find(description)
        {
            let snippet = snippet_around_match(description, mat.start(), mat.end(), 32);
            if !snippet.is_empty() {
                snippets.insert(issue.id.clone(), snippet);
                continue;
            }
        }

        if regex.is_match(&issue.id) && !regex.is_match(&issue.title) {
            snippets.insert(issue.id.clone(), "ID match".to_string());
        }
    }

    snippets
}

fn build_highlight_regex(query: &str) -> Option<Regex> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let pattern = regex::escape(trimmed);
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

fn snippet_around_match(text: &str, start: usize, end: usize, radius: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);

    let mut char_starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    char_starts.push(text.len());

    let total_chars = char_starts.len().saturating_sub(1);
    let start_char = char_starts.partition_point(|&idx| idx < start);
    let end_char = char_starts.partition_point(|&idx| idx < end);

    let snippet_start_char = start_char.saturating_sub(radius);
    let snippet_end_char = (end_char + radius).min(total_chars);

    let (Some(&snippet_start_byte), Some(&snippet_end_byte)) = (
        char_starts.get(snippet_start_char),
        char_starts.get(snippet_end_char),
    ) else {
        return String::new();
    };

    let Some(snippet_slice) = text.get(snippet_start_byte..snippet_end_byte) else {
        return String::new();
    };
    let mut snippet = snippet_slice.trim().to_string();
    snippet = normalize_whitespace(&snippet);
    if snippet.is_empty() {
        return snippet;
    }

    if snippet_start_char > 0 {
        snippet.insert_str(0, "...");
    }
    if snippet_end_char < total_chars {
        snippet.push_str("...");
    }

    snippet
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_filters(args: &ListArgs) -> Result<ListFilters> {
    validate_sort_key(args.sort.as_deref())?;

    // `--status all` is the same meta-value `br lint` accepts: no status
    // filter, every status included (beads_rust-6ilv).
    let all_statuses = super::status_filter_requests_all(&args.status);
    let statuses = if args.status.is_empty() || all_statuses {
        None
    } else {
        Some(
            args.status
                .iter()
                .map(|s| s.parse())
                .collect::<Result<Vec<Status>>>()?,
        )
    };

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

    let priorities = if args.priority.is_empty() {
        None
    } else {
        let mut parsed = Vec::new();
        for p in &args.priority {
            parsed.push(Priority::from_str(p)?);
        }
        Some(parsed)
    };

    let include_closed = args.all
        || all_statuses
        || statuses
            .as_ref()
            .is_some_and(|parsed| parsed.iter().any(Status::is_terminal));

    // Deferred issues are included by default (consistent with "open" status semantics).
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
        // #349: search stays capped (a broad text query can match a large
        // fraction of the corpus); list/ready are complete by default.
        limit: Some(args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT)),
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

fn needs_client_filters(args: &ListArgs) -> bool {
    !args.id.is_empty()
        || args.priority_min.is_some()
        || args.priority_max.is_some()
        || args.desc_contains.is_some()
        || args.notes_contains.is_some()
        || args.deferred
        || args.overdue
}

fn requires_post_query_ordering(_args: &ListArgs, client_filters: bool) -> bool {
    client_filters
}

fn validate_sort_key(sort: Option<&str>) -> Result<()> {
    let Some(sort_key) = sort else {
        return Ok(());
    };

    match sort_key {
        "priority" | "created_at" | "created" | "updated_at" | "updated" | "title" => Ok(()),
        _ => Err(BeadsError::Validation {
            field: "sort".to_string(),
            reason: format!("invalid sort field '{sort_key}'"),
        }),
    }
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

    // Use Regex for efficient case-insensitive search without full description allocations
    let desc_regex = args.desc_contains.as_deref().and_then(|needle| {
        RegexBuilder::new(&regex::escape(needle))
            .case_insensitive(true)
            .build()
            .ok()
    });
    let notes_regex = args.notes_contains.as_deref().and_then(|needle| {
        RegexBuilder::new(&regex::escape(needle))
            .case_insensitive(true)
            .build()
            .ok()
    });

    // Deferred issues are included by default when no status filter is specified,
    // except `--overdue` keeps deferred work hidden unless requested.
    let include_deferred = args.deferred
        || args.all
        || (!args.overdue && args.status.is_empty())
        || args
            .status
            .iter()
            .any(|status| status.eq_ignore_ascii_case("deferred"));

    if let Some(min) = min_priority
        && !(0..=4).contains(&min)
    {
        return Err(BeadsError::InvalidPriority {
            priority: min.to_string(),
        });
    }
    if let Some(max) = max_priority
        && !(0..=4).contains(&max)
    {
        return Err(BeadsError::InvalidPriority {
            priority: max.to_string(),
        });
    }

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

        if let Some(ref re) = desc_regex {
            let haystack = issue.description.as_deref().unwrap_or("");
            if !re.is_match(haystack) {
                continue;
            }
        }

        if let Some(ref re) = notes_regex {
            let haystack = issue.notes.as_deref().unwrap_or("");
            if !re.is_match(haystack) {
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

fn apply_sort_by_issue<T>(
    items: &mut [T],
    sort: Option<&str>,
    reverse: bool,
    mut issue_of: impl FnMut(&T) -> &crate::model::Issue,
) -> Result<()> {
    match sort {
        None | Some("priority") => {
            if reverse {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (
                        std::cmp::Reverse(issue.priority),
                        issue.created_at,
                        issue.id.clone(),
                    )
                });
            } else {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (
                        issue.priority,
                        std::cmp::Reverse(issue.created_at),
                        issue.id.clone(),
                    )
                });
            }
        }
        Some("created_at" | "created") => {
            if reverse {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (issue.created_at, issue.id.clone())
                });
            } else {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (std::cmp::Reverse(issue.created_at), issue.id.clone())
                });
            }
        }
        Some("updated_at" | "updated") => {
            if reverse {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (issue.updated_at, issue.id.clone())
                });
            } else {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (std::cmp::Reverse(issue.updated_at), issue.id.clone())
                });
            }
        }
        Some("title") => {
            if reverse {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (
                        std::cmp::Reverse(issue.title.to_lowercase()),
                        issue.id.clone(),
                    )
                });
            } else {
                items.sort_by_cached_key(|item| {
                    let issue = issue_of(item);
                    (issue.title.to_lowercase(), issue.id.clone())
                });
            }
        }
        Some(sort_key) => {
            return Err(BeadsError::Validation {
                field: "sort".to_string(),
                reason: format!("invalid sort field '{sort_key}'"),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
fn apply_sort(issues: &mut [IssueWithCounts], sort: Option<&str>, reverse: bool) -> Result<()> {
    apply_sort_by_issue(issues, sort, reverse, |issue| &issue.issue)
}

fn apply_issue_sort(
    issues: &mut [crate::model::Issue],
    sort: Option<&str>,
    reverse: bool,
) -> Result<()> {
    apply_sort_by_issue(issues, sort, reverse, |issue| issue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{DateTime, TimeZone, Utc};

    fn make_issue(
        id: &str,
        title: &str,
        description: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: description.map(str::to_string),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at,
            created_by: None,
            updated_at: created_at,
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
    fn test_snippet_around_match_clamps_invalid_offsets() {
        assert_eq!(snippet_around_match("alpha beta", 999, 1000, 0), "");

        let snippet = snippet_around_match("alpha beta", 999, 1000, 5);
        assert_eq!(snippet, "...beta");

        let multibyte = snippet_around_match("alpha βeta", 7, 6, 2);
        assert_eq!(multibyte, "...βet...");
    }

    #[test]
    fn test_search_matches_title_description_id() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 1, 3, 0, 0, 0).unwrap();

        let issue1 = make_issue("bd-001", "Alpha title", None, t1);
        let issue2 = make_issue("bd-002", "Other", Some("alpha desc"), t2);
        let issue3 = make_issue("bd-xyz", "Other", None, t3);

        storage.create_issue(&issue1, "tester").expect("create");
        storage.create_issue(&issue2, "tester").expect("create");
        storage.create_issue(&issue3, "tester").expect("create");

        let filters = ListFilters::default();
        let results = storage.search_issues("alpha", &filters).expect("search");
        let ids: Vec<String> = results.into_iter().map(|issue| issue.id).collect();
        assert!(ids.contains(&"bd-001".to_string()));
        assert!(ids.contains(&"bd-002".to_string()));
        assert!(!ids.contains(&"bd-xyz".to_string()));

        let results = storage.search_issues("xyz", &filters).expect("search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-xyz");
    }

    #[test]
    fn test_search_matches_comment_bodies() {
        // beads_rust#416: a token that exists only in a comment body must be
        // findable — agent workflows put durable handoffs in comments.
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let commented = make_issue(
            "bd-101",
            "comment search boundary",
            Some("control text"),
            t1,
        );
        let control = make_issue("bd-102", "unrelated", None, t2);
        storage.create_issue(&commented, "tester").expect("create");
        storage.create_issue(&control, "tester").expect("create");
        storage
            .add_comment("bd-101", "tester", "Comment-Only-Token-20260807 lives here")
            .expect("comment");

        let filters = ListFilters::default();

        // Case-insensitive comment-body match on the default path.
        let results = storage
            .search_issues("comment-only-token-20260807", &filters)
            .expect("search");
        let ids: Vec<String> = results.into_iter().map(|issue| issue.id).collect();
        assert_eq!(ids, vec!["bd-101".to_string()]);

        // The default-visible limited-page fast path must agree.
        let limited = ListFilters {
            limit: Some(10),
            ..ListFilters::default()
        };
        let results = storage
            .search_issues("comment-only-token-20260807", &limited)
            .expect("limited search");
        let ids: Vec<String> = results.into_iter().map(|issue| issue.id).collect();
        assert_eq!(ids, vec!["bd-101".to_string()]);

        // A token in no comment or field still returns nothing.
        assert!(
            storage
                .search_issues("token-that-matches-nothing", &filters)
                .expect("empty search")
                .is_empty()
        );
    }

    #[test]
    fn issue_with_counts_applies_relation_metadata() {
        let issue = make_issue("bd-001", "Search result", None, Utc::now());
        let mut relation_metadata = SearchRelationMetadata {
            labels_by_id: std::collections::HashMap::from([(
                "bd-001".to_string(),
                vec!["perf".to_string(), "search".to_string()],
            )]),
            dependency_counts: std::collections::HashMap::from([("bd-001".to_string(), 2)]),
            dependent_counts: std::collections::HashMap::from([("bd-001".to_string(), 3)]),
        };

        let output = issue_with_counts(issue, &mut relation_metadata);

        assert_eq!(output.issue.labels, vec!["perf", "search"]);
        assert_eq!(output.dependency_count, 2);
        assert_eq!(output.dependent_count, 3);
        assert!(relation_metadata.labels_by_id.is_empty());
    }

    #[test]
    fn test_label_filters_do_not_require_client_filtering() {
        let args = ListArgs {
            label: vec!["backend".to_string()],
            ..ListArgs::default()
        };

        assert!(!needs_client_filters(&args));
    }

    #[test]
    fn test_label_any_filters_do_not_require_client_filtering() {
        let args = ListArgs {
            label_any: vec!["backend".to_string(), "ops".to_string()],
            ..ListArgs::default()
        };

        assert!(!needs_client_filters(&args));
    }

    #[test]
    fn test_search_build_filters_applies_search_defaults_when_cli_omits_pagination() {
        // #349: search stays capped at DEFAULT_SEARCH_LIMIT (50) by default,
        // even though list/ready became unlimited.
        let filters = build_filters(&ListArgs::default()).expect("build filters");

        assert_eq!(filters.limit, Some(DEFAULT_SEARCH_LIMIT));
        assert_eq!(filters.offset, Some(DEFAULT_LIST_OFFSET));
    }

    #[test]
    fn test_requires_post_query_ordering_only_for_client_filters() {
        let args = ListArgs::default();
        assert!(!requires_post_query_ordering(&args, false));

        let args = ListArgs {
            reverse: true,
            ..ListArgs::default()
        };
        assert!(!requires_post_query_ordering(&args, false));

        let args = ListArgs {
            sort: Some("updated".to_string()),
            ..ListArgs::default()
        };
        assert!(!requires_post_query_ordering(&args, false));

        let args = ListArgs {
            desc_contains: Some("needle".to_string()),
            ..ListArgs::default()
        };
        assert!(requires_post_query_ordering(&args, true));
    }

    #[test]
    fn test_search_rejects_invalid_sort_without_client_filters() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let timestamp = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let issue = make_issue("bd-001", "Match title", Some("match body"), timestamp);
        storage.create_issue(&issue, "tester").expect("create");

        let args = ListArgs {
            sort: Some("bogus".to_string()),
            ..ListArgs::default()
        };

        let err = collect_search_results(&storage, "match", &args)
            .expect_err("invalid sort keys must be rejected before storage fallback");

        assert!(
            matches!(
                err,
                BeadsError::Validation { ref field, ref reason }
                    if field == "sort" && reason.contains("bogus")
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_search_respects_sort_before_limit() {
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 1, 3, 0, 0, 0).unwrap();

        let mut newer_created_but_older_updated = make_issue("bd-a", "Alpha", None, t3);
        newer_created_but_older_updated.updated_at = t1;

        let mut older_created_but_newer_updated = make_issue("bd-b", "Beta", None, t1);
        older_created_but_newer_updated.updated_at = t3;

        let mut items = vec![
            newer_created_but_older_updated,
            older_created_but_newer_updated,
        ];

        apply_issue_sort(&mut items, Some("updated"), false).expect("sort");
        items.truncate(1);
        assert_eq!(items[0].id, "bd-b");
    }

    #[test]
    fn test_search_applies_offset_after_explicit_sort() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 1, 3, 0, 0, 0).unwrap();

        let alpha = make_issue("bd-alpha", "match Alpha", None, t2);
        let bravo = make_issue("bd-bravo", "match Bravo", None, t1);
        let zulu = make_issue("bd-zulu", "match Zulu", None, t3);

        storage.create_issue(&alpha, "tester").expect("create");
        storage.create_issue(&bravo, "tester").expect("create");
        storage.create_issue(&zulu, "tester").expect("create");

        let args = ListArgs {
            sort: Some("title".to_string()),
            offset: Some(1),
            limit: Some(1),
            ..Default::default()
        };

        let results = collect_search_results(&storage, "match", &args).expect("search");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-bravo");
    }

    #[test]
    fn test_search_applies_offset_after_client_filters() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 1, 3, 0, 0, 0).unwrap();

        let older_match = make_issue("bd-old", "match old", Some("needle"), t1);
        let newer_match = make_issue("bd-mid", "match mid", Some("needle"), t2);
        let newest_nonmatch = make_issue("bd-new", "match new", Some("other"), t3);

        storage
            .create_issue(&older_match, "tester")
            .expect("create");
        storage
            .create_issue(&newer_match, "tester")
            .expect("create");
        storage
            .create_issue(&newest_nonmatch, "tester")
            .expect("create");

        let args = ListArgs {
            desc_contains: Some("needle".to_string()),
            offset: Some(1),
            limit: Some(1),
            ..Default::default()
        };

        let results = collect_search_results(&storage, "match", &args).expect("search");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-old");
    }

    #[test]
    fn test_search_overdue_excludes_deferred_unless_requested() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let created_at = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let overdue_at = Utc::now() - chrono::Duration::days(1);

        let mut open_overdue = make_issue("bd-open", "match open overdue", None, created_at);
        open_overdue.due_at = Some(overdue_at);

        let mut deferred_overdue =
            make_issue("bd-deferred", "match deferred overdue", None, created_at);
        deferred_overdue.status = Status::Deferred;
        deferred_overdue.due_at = Some(overdue_at);

        for issue in [open_overdue, deferred_overdue] {
            storage.create_issue(&issue, "tester").expect("create");
        }

        let overdue_only = collect_search_results(
            &storage,
            "match",
            &ListArgs {
                overdue: true,
                ..Default::default()
            },
        )
        .expect("search overdue");
        let overdue_only_ids: Vec<_> = overdue_only.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(overdue_only_ids, vec!["bd-open"]);

        let overdue_with_deferred = collect_search_results(
            &storage,
            "match",
            &ListArgs {
                overdue: true,
                deferred: true,
                ..Default::default()
            },
        )
        .expect("search overdue with deferred");
        let overdue_with_deferred_ids: Vec<_> = overdue_with_deferred
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(overdue_with_deferred_ids, vec!["bd-deferred", "bd-open"]);

        let overdue_with_all = collect_search_results(
            &storage,
            "match",
            &ListArgs {
                overdue: true,
                all: true,
                ..Default::default()
            },
        )
        .expect("search overdue with all");
        let overdue_with_all_ids: Vec<_> = overdue_with_all
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(overdue_with_all_ids, vec!["bd-deferred", "bd-open"]);
    }

    #[test]
    fn test_search_respects_reverse_without_explicit_sort() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let older_high_priority = make_issue("bd-old", "match old", None, t1);
        let newer_low_priority = make_issue("bd-new", "match new", None, t2);

        storage
            .create_issue(&older_high_priority, "tester")
            .expect("create");
        storage
            .create_issue(&newer_low_priority, "tester")
            .expect("create");

        let forward = storage
            .search_issues("match", &ListFilters::default())
            .expect("search");
        let reverse = storage
            .search_issues(
                "match",
                &ListFilters {
                    reverse: true,
                    ..Default::default()
                },
            )
            .expect("search");

        assert_eq!(forward[0].id, "bd-new");
        assert_eq!(forward[1].id, "bd-old");
        assert_eq!(reverse[0].id, "bd-old");
        assert_eq!(reverse[1].id, "bd-new");
    }

    #[test]
    fn test_search_command_pipeline_applies_reverse_once() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let older = make_issue("bd-old", "match old", None, t1);
        let newer = make_issue("bd-new", "match new", None, t2);
        storage.create_issue(&older, "tester").expect("create");
        storage.create_issue(&newer, "tester").expect("create");

        let args = ListArgs {
            reverse: true,
            ..Default::default()
        };
        let mut filters = build_filters(&args).expect("filters");
        let limit = filters.limit.take();
        filters.sort = None;
        filters.reverse = false;

        let mut issues = storage.search_issues("match", &filters).expect("search");

        apply_issue_sort(&mut issues, args.sort.as_deref(), args.reverse).expect("sort");
        if let Some(limit) = limit {
            issues.truncate(limit);
        }

        assert_eq!(issues[0].id, "bd-old");
        assert_eq!(issues[1].id, "bd-new");
    }

    #[test]
    fn test_search_client_filter_reverse_keeps_id_tiebreaker_ascending() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let timestamp = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        for issue in [
            make_issue("bd-tie-b", "match tie b", Some("needle"), timestamp),
            make_issue("bd-tie-a", "match tie a", Some("needle"), timestamp),
        ] {
            storage.create_issue(&issue, "tester").expect("create");
        }

        let args = ListArgs {
            desc_contains: Some("needle".to_string()),
            reverse: true,
            limit: Some(2),
            ..Default::default()
        };

        let results = collect_search_results(&storage, "match", &args).expect("search");
        let ids: Vec<_> = results.iter().map(|issue| issue.id.as_str()).collect();

        assert_eq!(ids, vec!["bd-tie-a", "bd-tie-b"]);
    }

    #[test]
    fn test_sort_by_title_and_reverse() {
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a", "Alpha", None, t1);
        let issue_b = make_issue("bd-b", "Beta", None, t2);

        let mut items = vec![
            IssueWithCounts {
                issue: issue_b,
                dependency_count: 0,
                dependent_count: 0,
            },
            IssueWithCounts {
                issue: issue_a,
                dependency_count: 0,
                dependent_count: 0,
            },
        ];

        apply_sort(&mut items, Some("title"), false).expect("sort");
        assert_eq!(items[0].issue.title, "Alpha");
        apply_sort(&mut items, Some("title"), true).expect("sort");
        assert_eq!(items[0].issue.title, "Beta");
    }

    #[test]
    fn test_sort_created_at_desc_default() {
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let issue_old = make_issue("bd-old", "Old", None, t1);
        let issue_new = make_issue("bd-new", "New", None, t2);

        let mut items = vec![
            IssueWithCounts {
                issue: issue_old,
                dependency_count: 0,
                dependent_count: 0,
            },
            IssueWithCounts {
                issue: issue_new,
                dependency_count: 0,
                dependent_count: 0,
            },
        ];

        apply_sort(&mut items, Some("created_at"), false).expect("sort");
        assert_eq!(items[0].issue.id, "bd-new");
    }

    #[test]
    fn test_attach_counts_backfills_labels() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let issue = Issue {
            id: "bd-labeled".to_string(),
            title: "Labeled issue".to_string(),
            ..Issue::default()
        };
        storage.create_issue(&issue, "tester").expect("create");
        storage
            .add_label("bd-labeled", "backend", "tester")
            .expect("label");

        let stored_issue = storage
            .get_issue("bd-labeled")
            .expect("get")
            .expect("issue exists");
        assert!(
            stored_issue.labels.is_empty(),
            "labels are loaded separately"
        );

        let hydrated = attach_counts(&storage, vec![stored_issue]).expect("attach counts");
        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].issue.labels, vec!["backend".to_string()]);
    }
}
