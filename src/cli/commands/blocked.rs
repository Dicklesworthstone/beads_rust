//! `br blocked` command implementation.
//!
//! Lists blocked issues from the `blocked_issues_cache`.

use super::auto_import_external_projects_if_stale;
use crate::cli::{BlockedArgs, OutputFormat, resolve_output_format_basic_with_outer_mode};
use crate::config::{
    CliOverrides, discover_beads_dir_with_cli, external_project_db_paths, open_storage_with_cli,
    should_use_color,
};
use crate::error::Result;
use crate::format::{BlockedIssue, BlockedIssueOutput, sanitize_terminal_inline};
use crate::model::{IssueType, Priority};
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use std::path::Path;
use std::str::FromStr;

/// Execute the blocked command.
///
/// # Errors
///
/// Returns an error if:
/// - The beads directory cannot be found
/// - The database cannot be opened
/// - Querying blocked issues fails
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &BlockedArgs,
    _json: bool,
    overrides: &CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    tracing::info!("Fetching blocked issues from cache");

    let beads_dir = discover_beads_dir_with_cli(overrides)?;
    execute_inner(args, overrides, outer_ctx, &beads_dir, None, None)
}

/// Execute blocked using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if querying blocked issues fails.
pub fn execute_with_storage(
    args: &BlockedArgs,
    overrides: &CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage: &SqliteStorage,
) -> Result<()> {
    execute_inner(args, overrides, outer_ctx, beads_dir, Some(storage), None)
}

/// Execute blocked using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error if querying blocked issues fails.
pub fn execute_with_storage_ctx(
    args: &BlockedArgs,
    overrides: &CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &crate::config::OpenStorageResult,
) -> Result<()> {
    execute_inner(
        args,
        overrides,
        outer_ctx,
        beads_dir,
        None,
        Some(storage_ctx),
    )
}

#[allow(clippy::too_many_lines)]
fn execute_inner(
    args: &BlockedArgs,
    overrides: &CliOverrides,
    outer_ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage: Option<&SqliteStorage>,
    preloaded_storage_ctx: Option<&crate::config::OpenStorageResult>,
) -> Result<()> {
    let owned_storage_ctx = if preloaded_storage.is_some() || preloaded_storage_ctx.is_some() {
        None
    } else {
        Some(open_storage_with_cli(beads_dir, overrides)?)
    };
    let storage = preloaded_storage
        .or_else(|| preloaded_storage_ctx.map(|ctx| &ctx.storage))
        .or_else(|| owned_storage_ctx.as_ref().map(|ctx| &ctx.storage))
        .expect("blocked should have an open storage handle");
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        args.robot,
    );
    let quiet = overrides.quiet.unwrap_or(false);
    let fast_ctx = OutputContext::from_output_format(output_format, quiet, true);
    if matches!(output_format, OutputFormat::Json | OutputFormat::Toon)
        && !storage.may_have_blocked_command_results()?
    {
        let blocked_issues = Vec::new();
        output_structured_blocked(args, output_format, &fast_ctx, &blocked_issues);
        return Ok(());
    }

    let has_external_dependencies = storage.has_external_dependencies(true)?;
    let needs_config = has_external_dependencies
        || matches!(output_format, OutputFormat::Text | OutputFormat::Csv);
    let config_layer = if needs_config {
        Some(
            if let Some(storage_ctx) = preloaded_storage_ctx.or(owned_storage_ctx.as_ref()) {
                storage_ctx.load_config(overrides)?
            } else {
                crate::config::load_config(beads_dir, Some(storage), overrides)?
            },
        )
    } else {
        None
    };
    let use_color = matches!(output_format, OutputFormat::Text | OutputFormat::Csv)
        && config_layer.as_ref().is_some_and(should_use_color);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);

    // Get blocked issues from cache
    let blocked_raw = storage.get_blocked_issues_for_command_output()?;

    tracing::debug!(
        count = blocked_raw.len(),
        "Found {} blocked issues",
        blocked_raw.len()
    );

    // Convert to BlockedIssue format
    let mut blocked_issues: Vec<BlockedIssue> = blocked_raw
        .into_iter()
        .map(|(issue, blockers)| BlockedIssue {
            blocked_by_count: blockers.len(),
            blocked_by: blockers,
            issue,
        })
        .collect();

    if has_external_dependencies {
        let config_layer = config_layer
            .as_ref()
            .expect("external dependencies require config");
        auto_import_external_projects_if_stale(config_layer, beads_dir, overrides);
        let external_db_paths = external_project_db_paths(config_layer, beads_dir);
        let external_statuses =
            storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
        let mut external_blockers = storage.external_blockers(&external_statuses)?;
        if !external_blockers.is_empty() {
            let mut by_id: std::collections::HashMap<String, usize> = blocked_issues
                .iter()
                .enumerate()
                .map(|(idx, bi)| (bi.issue.id.clone(), idx))
                .collect();

            let mut external_ids_to_fetch = Vec::new();
            for (issue_id, blockers) in &external_blockers {
                if let Some(entry) = by_id
                    .get(issue_id)
                    .and_then(|idx| blocked_issues.get_mut(*idx))
                {
                    entry.blocked_by.extend(blockers.clone());
                    entry.blocked_by.sort();
                    entry.blocked_by.dedup();
                    entry.blocked_by_count = entry.blocked_by.len();
                } else {
                    external_ids_to_fetch.push(issue_id.clone());
                }
            }

            if !external_ids_to_fetch.is_empty() {
                let fetched_issues = storage.get_issues_by_ids(&external_ids_to_fetch)?;
                for issue in fetched_issues {
                    if !include_in_blocked_list(&issue.status) {
                        continue;
                    }
                    if let Some(blockers) = external_blockers.remove(&issue.id) {
                        let blocked_by_count = blockers.len();
                        let issue_id = issue.id.clone();
                        blocked_issues.push(BlockedIssue {
                            blocked_by_count,
                            blocked_by: blockers,
                            issue,
                        });
                        by_id.insert(issue_id, blocked_issues.len() - 1);
                    }
                }
            }
        }
    }

    // Apply filters
    filter_by_type(&mut blocked_issues, &args.type_)?;
    filter_by_priority(&mut blocked_issues, &args.priority)?;

    // Filter by labels (AND logic) - need to fetch labels from storage
    if !args.label.is_empty() {
        filter_by_labels(&mut blocked_issues, storage, &args.label)?;
    }

    // Sort by priority (ascending), then by blocker count (descending)
    sort_blocked_issues(&mut blocked_issues);

    // Apply limit
    if args.limit > 0 && blocked_issues.len() > args.limit {
        blocked_issues.truncate(args.limit);
    }

    for bi in &blocked_issues {
        tracing::trace!(
            id = %bi.issue.id,
            blockers = ?bi.blocked_by,
            "Blocked issue: {} blocked by {:?}",
            bi.issue.id,
            bi.blocked_by
        );
    }

    // Output
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    match output_format {
        OutputFormat::Json | OutputFormat::Toon => {
            output_structured_blocked(args, output_format, &ctx, &blocked_issues);
        }
        OutputFormat::Text | OutputFormat::Csv => {
            let max_width = if args.wrap { ctx.width() } else { 0 };
            if matches!(ctx.mode(), OutputMode::Rich) {
                render_blocked_rich(&blocked_issues, args.detailed, storage, max_width);
            } else {
                print_text_output(&blocked_issues, args.detailed, storage, max_width);
            }
        }
    }

    Ok(())
}

fn include_in_blocked_list(status: &crate::model::Status) -> bool {
    !status.is_terminal()
}

fn blocked_issue_outputs(blocked_issues: &[BlockedIssue]) -> Vec<BlockedIssueOutput> {
    blocked_issues.iter().map(blocked_issue_output).collect()
}

fn output_structured_blocked(
    args: &BlockedArgs,
    output_format: OutputFormat,
    ctx: &OutputContext,
    blocked_issues: &[BlockedIssue],
) {
    match output_format {
        OutputFormat::Json => ctx.json_array(blocked_issues.iter().map(blocked_issue_output)),
        OutputFormat::Toon => {
            let output = blocked_issue_outputs(blocked_issues);
            ctx.toon_with_stats(&output, args.stats);
        }
        OutputFormat::Text | OutputFormat::Csv => {}
    }
}

fn blocked_issue_output(bi: &BlockedIssue) -> BlockedIssueOutput {
    BlockedIssueOutput {
        blocked_by: bi
            .blocked_by
            .iter()
            .map(|blocker_ref| blocker_id_from_ref(blocker_ref).to_string())
            .collect(),
        blocked_by_count: bi.blocked_by_count,
        created_at: bi.issue.created_at,
        created_by: bi.issue.created_by.clone(),
        description: bi.issue.description.clone(),
        id: bi.issue.id.clone(),
        issue_type: bi.issue.issue_type.clone(),
        priority: bi.issue.priority,
        status: bi.issue.status.clone(),
        title: bi.issue.title.clone(),
        updated_at: bi.issue.updated_at,
    }
}

/// Sort blocked issues by priority (ascending), then by blocker count (descending).
fn sort_blocked_issues(issues: &mut [BlockedIssue]) {
    issues.sort_by(|a, b| {
        let pa = a.issue.priority.0;
        let pb = b.issue.priority.0;
        pa.cmp(&pb)
            .then_with(|| b.blocked_by_count.cmp(&a.blocked_by_count))
            .then_with(|| b.issue.created_at.cmp(&a.issue.created_at))
            .then_with(|| a.issue.id.cmp(&b.issue.id))
    });
}

/// Filter blocked issues by issue type (case-insensitive).
fn filter_by_type(issues: &mut Vec<BlockedIssue>, types: &[String]) -> Result<()> {
    if types.is_empty() {
        return Ok(());
    }

    let parsed = types
        .iter()
        .map(|t| IssueType::from_str(t))
        .collect::<Result<Vec<IssueType>>>()?;

    issues.retain(|bi| parsed.contains(&bi.issue.issue_type));
    Ok(())
}

/// Filter blocked issues by priority.
fn filter_by_priority(issues: &mut Vec<BlockedIssue>, priorities: &[String]) -> Result<()> {
    if priorities.is_empty() {
        return Ok(());
    }

    let parsed = priorities
        .iter()
        .map(|p| Priority::from_str(p))
        .collect::<Result<Vec<Priority>>>()?;

    issues.retain(|bi| parsed.contains(&bi.issue.priority));
    Ok(())
}

fn filter_by_labels(
    issues: &mut Vec<BlockedIssue>,
    storage: &crate::storage::SqliteStorage,
    labels: &[String],
) -> Result<()> {
    let mut filtered = Vec::with_capacity(issues.len());
    let issue_ids: Vec<String> = issues.iter().map(|bi| bi.issue.id.clone()).collect();
    let labels_map = storage.get_labels_for_issues(&issue_ids)?;

    for issue in issues.drain(..) {
        let matches = labels_map
            .get(&issue.issue.id)
            .is_some_and(|issue_labels| labels.iter().all(|l| issue_labels.contains(l)));
        if matches {
            filtered.push(issue);
        }
    }
    *issues = filtered;
    Ok(())
}

fn print_text_output(
    blocked_issues: &[BlockedIssue],
    verbose: bool,
    storage: &crate::storage::SqliteStorage,
    max_width: usize,
) {
    use crate::format::{format_status_label, sanitize_terminal_inline, truncate_title};

    if blocked_issues.is_empty() {
        // Match bd format
        println!("✨ No blocked issues");
        return;
    }

    // Match bd format: 🚫 Blocked issues (N):
    println!("\n🚫 Blocked issues ({}):\n", blocked_issues.len());

    for bi in blocked_issues {
        let priority = bi.issue.priority.0;
        let issue_id = blocked_id_text(&bi.issue.id);
        // Calculate prefix length for title truncation
        // "[● P2] id: " prefix - estimate ~20 chars for priority badge and ID
        let prefix_len = 10 + issue_id.len();
        let title = if max_width == 0 {
            // No wrap - use full title
            sanitize_terminal_inline(&bi.issue.title).into_owned()
        } else {
            // When wrapping, truncate for initial display
            truncate_title(&bi.issue.title, max_width.saturating_sub(prefix_len))
        };
        // Match bd format: [● P2] ID: Title
        println!("[● P{}] {}: {}", priority, issue_id, title);

        if verbose {
            println!("  Blocked by:");
            for blocker_ref in &bi.blocked_by {
                // blocker_ref format is "id:status", extract just the id for lookup
                let blocker_id = blocker_id_from_ref(blocker_ref);
                if let Ok(Some(blocker)) = storage.get_issue(blocker_id) {
                    let blocker_title = if max_width > 0 {
                        truncate_title(&blocker.title, max_width.saturating_sub(30))
                    } else {
                        sanitize_terminal_inline(&blocker.title).into_owned()
                    };
                    println!(
                        "    • {}: {} [P{}] [{}]",
                        blocked_id_text(blocker_id),
                        blocker_title,
                        blocker.priority.0,
                        format_status_label(&blocker.status, false)
                    );
                } else {
                    println!(
                        "    • {} (not found)",
                        sanitize_terminal_inline(blocker_ref)
                    );
                }
            }
        } else {
            // Match bd format: Blocked by N open dependencies: [id1, id2]
            // Note: bd uses "dependencies" even for count=1 (grammatically incorrect but we match for conformance)
            let count = bi.blocked_by.len();
            // Extract just the IDs from blocker refs (strip :status suffix)
            println!(
                "  Blocked by {} open dependencies: [{}]",
                count,
                blocked_id_list_text(&bi.blocked_by)
            );
        }
    }
}

fn blocked_id_text(id: &str) -> String {
    sanitize_terminal_inline(id).into_owned()
}

fn blocked_id_list_text(blocked_by: &[String]) -> String {
    blocked_by
        .iter()
        .map(|r| blocked_id_text(blocker_id_from_ref(r)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn blocker_id_from_ref(blocker_ref: &str) -> &str {
    // Split from the right to preserve external IDs containing ':'
    blocker_ref
        .rsplit_once(':')
        .map_or(blocker_ref, |(prefix, _)| prefix)
}

fn render_blocked_rich(
    blocked_issues: &[BlockedIssue],
    verbose: bool,
    storage: &crate::storage::SqliteStorage,
    max_width: usize,
) {
    use crate::format::{format_status_label, sanitize_terminal_inline, truncate_title};
    use rich_rust::Text;
    use rich_rust::prelude::*;

    fn color(name: &str) -> Color {
        Color::parse(name).unwrap_or_default()
    }

    let console = Console::default();

    if blocked_issues.is_empty() {
        let mut text = Text::new("");
        text.append_styled("\u{2728} ", Style::new().color(color("green")));
        text.append_styled(
            "No blocked issues",
            Style::new().bold().color(color("green")),
        );
        console.print_renderable(&text);
        return;
    }

    // Header
    let mut header = Text::new("");
    header.append_styled("\u{1f6ab} ", Style::new().color(color("red")));
    header.append_styled("Blocked issues", Style::new().bold().color(color("red")));
    header.append_styled(&format!(" ({})", blocked_issues.len()), Style::new().dim());
    console.print_renderable(&header);
    console.print("");

    for bi in blocked_issues {
        let priority = bi.issue.priority.0;
        let blocker_count = bi.blocked_by_count;
        let issue_id = blocked_id_text(&bi.issue.id);

        // Color code blocker count: yellow(1), orange(2-3), red(4+)
        let count_style = match blocker_count {
            1 => Style::new().color(color("yellow")),
            2 | 3 => Style::new().color(color("bright_yellow")),
            _ => Style::new().color(color("red")),
        };

        // Calculate title width - account for "[P2] id: " prefix and " [N blockers]" suffix
        let prefix_len = 6 + issue_id.len() + 2; // "[P2] " + id + ": "
        let suffix_len = 15; // " [N blockers]"
        let title = if max_width > 0 {
            let available = max_width.saturating_sub(prefix_len + suffix_len);
            truncate_title(&bi.issue.title, available)
        } else {
            sanitize_terminal_inline(&bi.issue.title).into_owned()
        };

        let mut line = Text::new("");
        line.append_styled(
            &format!("[P{}] ", priority),
            Style::new().bold().color(color("magenta")),
        );
        line.append_styled(&issue_id, Style::new().bold().color(color("cyan")));
        line.append(": ");
        line.append(&title);
        line.append_styled(&format!(" [{} blockers]", blocker_count), count_style);
        console.print_renderable(&line);

        if verbose {
            let mut blocked_label = Text::new("");
            blocked_label.append_styled("  Blocked by:", Style::new().dim());
            console.print_renderable(&blocked_label);

            for blocker_ref in &bi.blocked_by {
                let blocker_id = blocker_id_from_ref(blocker_ref);
                let mut blocker_line = Text::new("");
                blocker_line.append_styled("    \u{2022} ", Style::new().color(color("yellow")));
                blocker_line.append_styled(
                    &blocked_id_text(blocker_id),
                    Style::new().color(color("cyan")),
                );

                if let Ok(Some(blocker)) = storage.get_issue(blocker_id) {
                    let blocker_title = if max_width > 0 {
                        truncate_title(&blocker.title, max_width.saturating_sub(40))
                    } else {
                        sanitize_terminal_inline(&blocker.title).into_owned()
                    };
                    blocker_line.append(": ");
                    blocker_line.append(&blocker_title);
                    blocker_line
                        .append_styled(&format!(" [P{}]", blocker.priority.0), Style::new().dim());
                    blocker_line.append_styled(
                        &format!(" [{}]", format_status_label(&blocker.status, false)),
                        Style::new().dim(),
                    );
                } else {
                    blocker_line.append_styled(" (not found)", Style::new().dim());
                }
                console.print_renderable(&blocker_line);
            }
        } else {
            let mut detail = Text::new("");
            detail.append_styled("  Blocked by: ", Style::new().dim());
            detail.append_styled(
                &format!("[{}]", blocked_id_list_text(&bi.blocked_by)),
                Style::new().color(color("yellow")),
            );
            console.print_renderable(&detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BlockedArgs;
    use crate::logging::init_test_logging;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};
    use tracing::info;

    fn make_issue(id: &str, title: &str, priority: i32, issue_type: IssueType) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority(priority),
            issue_type,
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

    fn make_blocked_issue(
        id: &str,
        title: &str,
        priority: i32,
        blocker_count: usize,
    ) -> BlockedIssue {
        BlockedIssue {
            issue: make_issue(id, title, priority, IssueType::Task),
            blocked_by_count: blocker_count,
            blocked_by: (0..blocker_count).map(|i| format!("blocker-{i}")).collect(),
        }
    }

    #[test]
    fn test_blocked_args_defaults() {
        init_test_logging();
        info!("test_blocked_args_defaults: starting");
        // Note: Default::default() gives 0 for limit; clap sets 50 at parse time
        let args = BlockedArgs::default();
        assert_eq!(args.limit, 0); // Rust Default, not clap default
        assert!(!args.detailed);
        assert!(args.type_.is_empty());
        assert!(args.priority.is_empty());
        assert!(args.label.is_empty());
        assert!(!args.robot);
        info!("test_blocked_args_defaults: assertions passed");
    }

    #[test]
    fn test_blocked_issue_output_iterator_matches_materialized_outputs() {
        let issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P1", 1, 2),
        ];

        let streamed: Vec<BlockedIssueOutput> = issues.iter().map(blocked_issue_output).collect();
        let materialized = blocked_issue_outputs(&issues);

        let streamed_json =
            serde_json::to_vec(&streamed).expect("serialize streamed blocked output");
        let materialized_json =
            serde_json::to_vec(&materialized).expect("serialize materialized blocked output");
        assert_eq!(streamed_json, materialized_json);
    }

    #[test]
    fn test_blocked_id_display_helpers_escape_terminal_controls() {
        let id = blocked_id_text("bd-bad\x1b]52;c;bad\x07");
        assert!(!id.chars().any(char::is_control));
        assert_eq!(id, "bd-bad\\u{1b}]52;c;bad\\u{7}");

        let ids = blocked_id_list_text(&[
            "bd-one\x1b[2J:open".to_string(),
            "external:proj:\x08cap:blocked".to_string(),
        ]);

        assert!(!ids.chars().any(char::is_control));
        assert!(ids.contains("bd-one\\u{1b}[2J"));
        assert!(ids.contains("external:proj:\\u{8}cap"));
    }

    #[test]
    fn test_sort_by_priority_then_blocker_count() {
        init_test_logging();
        info!("test_sort_by_priority_then_blocker_count: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P2 few blockers", 2, 1),
            make_blocked_issue("b", "P1 few blockers", 1, 1),
            make_blocked_issue("c", "P1 many blockers", 1, 5),
            make_blocked_issue("d", "P0 critical", 0, 2),
        ];

        sort_blocked_issues(&mut issues);

        // Should be sorted: P0 first, then P1 (more blockers first), then P2
        assert_eq!(issues[0].issue.id, "d"); // P0
        assert_eq!(issues[1].issue.id, "c"); // P1, 5 blockers
        assert_eq!(issues[2].issue.id, "b"); // P1, 1 blocker
        assert_eq!(issues[3].issue.id, "a"); // P2
        info!("test_sort_by_priority_then_blocker_count: assertions passed");
    }

    #[test]
    fn test_sort_ties_by_created_at_desc_then_id() {
        let mut newer = make_blocked_issue("c", "newer", 1, 1);
        newer.issue.created_at = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();
        let older_a = make_blocked_issue("a", "older a", 1, 1);
        let older_b = make_blocked_issue("b", "older b", 1, 1);
        let mut issues = vec![older_b, newer, older_a];

        sort_blocked_issues(&mut issues);

        let ids: Vec<&str> = issues.iter().map(|issue| issue.issue.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
    }

    #[test]
    fn test_filter_by_type_empty_keeps_all() {
        init_test_logging();
        info!("test_filter_by_type_empty_keeps_all: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
        ];

        filter_by_type(&mut issues, &[]).expect("filter types");
        assert_eq!(issues.len(), 2);
        info!("test_filter_by_type_empty_keeps_all: assertions passed");
    }

    #[test]
    fn test_filter_by_type_filters_correctly() {
        init_test_logging();
        info!("test_filter_by_type_filters_correctly: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
            BlockedIssue {
                issue: make_issue("c", "Feature", 2, IssueType::Feature),
                blocked_by_count: 1,
                blocked_by: vec!["z".to_string()],
            },
        ];

        filter_by_type(&mut issues, &["bug".to_string()]).expect("filter types");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.id, "a");
        info!("test_filter_by_type_filters_correctly: assertions passed");
    }

    #[test]
    fn test_include_in_blocked_list_matches_local_blocked_query_statuses() {
        assert!(include_in_blocked_list(&Status::Open));
        assert!(include_in_blocked_list(&Status::InProgress));
        assert!(include_in_blocked_list(&Status::Deferred));
        assert!(include_in_blocked_list(&Status::Blocked));
        assert!(include_in_blocked_list(&Status::Pinned));
        assert!(include_in_blocked_list(&Status::Custom(
            "review".to_string()
        )));
        assert!(!include_in_blocked_list(&Status::Closed));
        assert!(!include_in_blocked_list(&Status::Tombstone));
    }

    #[test]
    fn test_filter_by_type_case_insensitive() {
        init_test_logging();
        info!("test_filter_by_type_case_insensitive: starting");
        let mut issues = vec![BlockedIssue {
            issue: make_issue("a", "Bug", 2, IssueType::Bug),
            blocked_by_count: 1,
            blocked_by: vec!["x".to_string()],
        }];

        filter_by_type(&mut issues, &["BUG".to_string()]).expect("filter types");
        assert_eq!(issues.len(), 1);

        let mut issues2 = vec![BlockedIssue {
            issue: make_issue("a", "Bug", 2, IssueType::Bug),
            blocked_by_count: 1,
            blocked_by: vec!["x".to_string()],
        }];

        filter_by_type(&mut issues2, &["Bug".to_string()]).expect("filter types");
        assert_eq!(issues2.len(), 1);
        info!("test_filter_by_type_case_insensitive: assertions passed");
    }

    #[test]
    fn test_filter_by_type_multiple_types() {
        init_test_logging();
        info!("test_filter_by_type_multiple_types: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
            BlockedIssue {
                issue: make_issue("c", "Feature", 2, IssueType::Feature),
                blocked_by_count: 1,
                blocked_by: vec!["z".to_string()],
            },
        ];

        filter_by_type(&mut issues, &["bug".to_string(), "feature".to_string()])
            .expect("filter types");
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.issue.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c"));
        info!("test_filter_by_type_multiple_types: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_empty_keeps_all() {
        init_test_logging();
        info!("test_filter_by_priority_empty_keeps_all: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[]).expect("filter priorities");
        assert_eq!(issues.len(), 3);
        info!("test_filter_by_priority_empty_keeps_all: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_single() {
        init_test_logging();
        info!("test_filter_by_priority_single: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &["2".to_string()]).expect("filter priorities");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.id, "b");
        info!("test_filter_by_priority_single: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_multiple() {
        init_test_logging();
        info!("test_filter_by_priority_multiple: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &["0".to_string(), "4".to_string()])
            .expect("filter priorities");
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.issue.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c"));
        info!("test_filter_by_priority_multiple: assertions passed");
    }
}
