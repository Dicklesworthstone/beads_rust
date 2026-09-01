//! Epic command implementation.

use crate::cli::{EpicCloseEligibleArgs, EpicCommands, EpicStatusArgs};
use crate::config;
use crate::error::Result;
use crate::format::sanitize_terminal_inline;
use crate::model::{EpicStatus, IssueType, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{IssueUpdate, ListFilters, SqliteStorage};
use chrono::Utc;
use crossterm::style::Stylize;
use rich_rust::prelude::*;
use serde::Serialize;

/// Execute the epic command.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub fn execute(
    command: &EpicCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    match command {
        EpicCommands::Status(args) => execute_status(args, json, cli, ctx),
        EpicCommands::CloseEligible(args) => execute_close_eligible(args, json, cli, ctx),
    }
}

/// Execute a read-only epic command using storage that was already opened by the caller.
///
/// Returns `Ok(false)` when the command needs the normal mutating path.
///
/// # Errors
///
/// Returns an error if database operations fail.
pub fn execute_with_storage_ctx(
    command: &EpicCommands,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    match command {
        EpicCommands::Status(args) => {
            execute_status_with_storage_ctx(args, cli, ctx, storage_ctx)?;
            Ok(true)
        }
        EpicCommands::CloseEligible(_) => Ok(false),
    }
}

fn execute_status(
    args: &EpicStatusArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_status_with_storage_ctx(args, cli, ctx, &storage_ctx)
}

fn execute_status_with_storage_ctx(
    args: &EpicStatusArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let config_layer = storage_ctx.load_config(cli)?;
    let use_color = config::should_use_color(&config_layer);

    let mut epics = load_epic_statuses(&storage_ctx.storage)?;
    if args.eligible_only {
        epics.retain(|e| e.eligible_for_close);
    }

    if ctx.is_toon() {
        ctx.toon(&epics);
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_pretty(&epics);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if epics.is_empty() {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_empty_epics_rich(ctx);
        } else {
            println!("No open epics found");
        }
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_epic_status_list_rich(&epics, ctx);
    } else {
        for epic_status in &epics {
            render_epic_status(epic_status, use_color);
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct CloseEligibleResult {
    closed: Vec<String>,
    count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
}

fn execute_close_eligible(
    args: &EpicCloseEligibleArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);

    let storage = &mut storage_ctx.storage;
    let mut epics = load_epic_statuses(storage)?;
    epics.retain(|e| e.eligible_for_close);

    if args.dry_run {
        if ctx.is_toon() {
            ctx.toon(&epics);
        } else if ctx.is_json() {
            ctx.json_pretty(&epics);
        } else if ctx.is_quiet() {
            return Ok(());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_rich(&epics, ctx);
        } else {
            println!("Would close {} epic(s):", epics.len());
            for epic_status in &epics {
                let id = format_epic_id(&epic_status.epic.id, false);
                let title = format_epic_title(&epic_status.epic.title, false);
                println!("  - {id}: {title}");
            }
        }
        return Ok(());
    }

    if epics.is_empty() {
        if ctx.is_toon() {
            let result = CloseEligibleResult {
                closed: Vec::new(),
                count: 0,
                warnings: Vec::new(),
            };
            ctx.toon(&result);
        } else if ctx.is_json() {
            let result = CloseEligibleResult {
                closed: Vec::new(),
                count: 0,
                warnings: Vec::new(),
            };
            ctx.json_pretty(&result);
        } else if ctx.is_quiet() {
            return Ok(());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_eligible_rich(ctx);
        } else {
            println!("No epics eligible for closure");
        }
        return Ok(());
    }

    let now = Utc::now();
    let reason = "All children completed";
    let closed_ids = close_eligible_epics_atomically(
        storage,
        &epics,
        &actor,
        now,
        reason,
        args.transition_comment.as_deref(),
    )?;
    let capacity_warnings = storage.take_capacity_warnings();

    storage_ctx.flush_no_db_if_dirty()?;

    if ctx.is_toon() {
        let result = CloseEligibleResult {
            closed: closed_ids.clone(),
            count: closed_ids.len(),
            warnings: capacity_warnings.clone(),
        };
        ctx.toon(&result);
    } else if ctx.is_json() {
        let result = CloseEligibleResult {
            closed: closed_ids.clone(),
            count: closed_ids.len(),
            warnings: capacity_warnings.clone(),
        };
        ctx.json_pretty(&result);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_close_result_rich(&closed_ids, ctx);
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    } else {
        println!("✓ Closed {} epic(s)", closed_ids.len());
        for id in &closed_ids {
            println!("  - {}", format_epic_id(id, false));
        }
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }
    Ok(())
}

fn close_eligible_epics_atomically(
    storage: &mut SqliteStorage,
    epics: &[EpicStatus],
    actor: &str,
    now: chrono::DateTime<Utc>,
    reason: &str,
    transition_comment: Option<&str>,
) -> Result<Vec<String>> {
    let updates = epics
        .iter()
        .map(|epic_status| {
            (
                epic_status.epic.id.clone(),
                IssueUpdate {
                    status: Some(Status::Closed),
                    closed_at: Some(Some(now)),
                    close_reason: Some(Some(reason.to_string())),
                    transition_comment: transition_comment.map(str::to_string),
                    ..Default::default()
                },
            )
        })
        .collect::<Vec<_>>();

    // The shared storage chokepoint preflights required fields, transition
    // gates, and final-state capacity for the entire batch before mutating any
    // epic. It then records comments, status/close events, and dirty markers in
    // that same transaction.
    storage.update_issues_atomically(&updates, actor)?;
    Ok(updates.into_iter().map(|(id, _)| id).collect())
}

fn load_epic_statuses(storage: &SqliteStorage) -> Result<Vec<EpicStatus>> {
    let filters = ListFilters {
        types: Some(vec![IssueType::Epic]),
        include_closed: false,
        include_deferred: true,
        ..Default::default()
    };
    let epics = storage.list_issues(&filters)?;
    if epics.is_empty() {
        return Ok(Vec::new());
    }

    let counts = storage.get_epic_counts()?;

    let mut statuses = Vec::new();
    for epic in epics {
        let (total_children, closed_children) = counts.get(&epic.id).copied().unwrap_or((0, 0));
        let eligible_for_close = total_children > 0 && closed_children == total_children;

        statuses.push(EpicStatus {
            epic,
            total_children,
            closed_children,
            eligible_for_close,
        });
    }

    Ok(statuses)
}

fn render_epic_status(epic_status: &EpicStatus, use_color: bool) {
    let total = epic_status.total_children;
    let closed = epic_status.closed_children;
    let percentage = completion_percentage(closed, total);
    let status_icon = render_status_icon(epic_status.eligible_for_close, percentage, use_color);

    let id = format_epic_id(&epic_status.epic.id, use_color);
    let title = format_epic_title(&epic_status.epic.title, use_color);

    println!("{status_icon} {id} {title}");
    println!("   Progress: {closed}/{total} children closed ({percentage}%)");
    if epic_status.eligible_for_close {
        let line = if use_color {
            "Eligible for closure".green().to_string()
        } else {
            "Eligible for closure".to_string()
        };
        println!("   {line}");
    }
    println!();
}

fn render_status_icon(eligible: bool, percentage: usize, use_color: bool) -> String {
    if eligible {
        if use_color {
            "✓".green().to_string()
        } else {
            "✓".to_string()
        }
    } else if percentage > 0 {
        if use_color {
            "○".yellow().to_string()
        } else {
            "○".to_string()
        }
    } else {
        "○".to_string()
    }
}

fn format_epic_id(id: &str, use_color: bool) -> String {
    let id = sanitize_terminal_inline(id);
    if use_color {
        id.as_ref().cyan().to_string()
    } else {
        id.into_owned()
    }
}

fn format_epic_title(title: &str, use_color: bool) -> String {
    let title = sanitize_terminal_inline(title);
    if use_color {
        title.as_ref().bold().to_string()
    } else {
        title.into_owned()
    }
}

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render the epic status list with rich formatting.
fn render_epic_status_list_rich(epics: &[EpicStatus], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    for (i, epic_status) in epics.iter().enumerate() {
        if i > 0 {
            content.append("\n");
        }

        let total = epic_status.total_children;
        let closed = epic_status.closed_children;
        let percentage = completion_percentage(closed, total);

        // Status icon
        if epic_status.eligible_for_close {
            content.append_styled("✓ ", theme.success.clone());
        } else if percentage > 0 {
            content.append_styled("○ ", theme.warning.clone());
        } else {
            content.append_styled("○ ", theme.dimmed.clone());
        }

        // ID and title
        content.append_styled(
            sanitize_terminal_inline(&epic_status.epic.id).as_ref(),
            theme.issue_id.clone(),
        );
        content.append(" ");
        content.append_styled(
            sanitize_terminal_inline(&epic_status.epic.title).as_ref(),
            theme.emphasis.clone(),
        );
        content.append("\n");

        // Progress bar
        content.append("   ");
        render_progress_bar(&mut content, closed, total, percentage, theme);
        content.append("\n");

        // Eligible notice
        if epic_status.eligible_for_close {
            content.append("   ");
            content.append_styled("Ready for closure", theme.success.clone());
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Epic Status", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render a progress bar inline.
fn render_progress_bar(
    content: &mut Text,
    closed: usize,
    total: usize,
    percentage: usize,
    theme: &crate::output::Theme,
) {
    let bar_width = 20;
    let filled = completion_bar_filled_width(closed, total, bar_width);
    let empty = bar_width - filled;

    content.append_styled("[", theme.dimmed.clone());
    if filled > 0 {
        content.append_styled(&"█".repeat(filled), theme.success.clone());
    }
    if empty > 0 {
        content.append_styled(&"░".repeat(empty), theme.dimmed.clone());
    }
    content.append_styled("] ", theme.dimmed.clone());

    content.append(&format!("{closed}/{total} "));
    content.append_styled(&format!("({percentage}%)"), theme.dimmed.clone());
}

fn completion_percentage(closed: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }

    let closed = closed.min(total) as u128;
    let total = total as u128;
    usize::try_from((closed * 100) / total).unwrap_or(100)
}

fn completion_bar_filled_width(closed: usize, total: usize, bar_width: usize) -> usize {
    if total == 0 {
        return 0;
    }

    let closed = closed.min(total) as u128;
    let total = total as u128;
    let bar_width = bar_width as u128;
    usize::try_from((closed * bar_width) / total).unwrap_or(usize::MAX)
}

/// Render empty epics message with rich formatting.
fn render_empty_epics_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("No open epics found", theme.dimmed.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Epic Status", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render no eligible epics message with rich formatting.
fn render_no_eligible_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("No epics eligible for closure", theme.dimmed.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Epic Close", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render dry-run results with rich formatting.
fn render_dry_run_rich(epics: &[EpicStatus], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled("⚡ Dry-run mode ", theme.warning.clone());
    content.append_styled("(no changes will be made)\n\n", theme.dimmed.clone());

    content.append(&format!(
        "Would close {} epic{}:\n\n",
        epics.len(),
        if epics.len() == 1 { "" } else { "s" }
    ));

    for epic_status in epics {
        content.append_styled("  • ", theme.dimmed.clone());
        content.append_styled(
            sanitize_terminal_inline(&epic_status.epic.id).as_ref(),
            theme.issue_id.clone(),
        );
        content.append(" ");
        content.append(sanitize_terminal_inline(&epic_status.epic.title).as_ref());
        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "Epic Close (Dry Run)",
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render close results with rich formatting.
fn render_close_result_rich(closed_ids: &[String], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled("✓ ", theme.success.clone());
    content.append_styled(
        &format!(
            "Closed {} epic{}\n",
            closed_ids.len(),
            if closed_ids.len() == 1 { "" } else { "s" }
        ),
        theme.success.clone(),
    );

    if !closed_ids.is_empty() {
        content.append("\n");
        for id in closed_ids {
            content.append_styled("  • ", theme.dimmed.clone());
            content.append_styled(
                sanitize_terminal_inline(id).as_ref(),
                theme.issue_id.clone(),
            );
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Epic Close", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, Priority, Status};
    use crate::storage::IssueUpdate;
    use chrono::TimeZone;

    fn base_issue(id: &str, title: &str, issue_type: IssueType, status: Status) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status,
            priority: Priority::MEDIUM,
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

    fn find_epic<'a>(epics: &'a [EpicStatus], id: &str) -> Option<&'a EpicStatus> {
        epics.iter().find(|e| e.epic.id == id)
    }

    #[test]
    fn epic_status_tracks_children_and_eligibility() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let epic = base_issue("bd-epic-1", "Epic", IssueType::Epic, Status::Open);
        let task1 = base_issue("bd-task-1", "Task 1", IssueType::Task, Status::Open);
        let task2 = base_issue("bd-task-2", "Task 2", IssueType::Task, Status::Open);

        storage.create_issue(&epic, "tester").unwrap();
        storage.create_issue(&task1, "tester").unwrap();
        storage.create_issue(&task2, "tester").unwrap();
        storage
            .add_dependency("bd-task-1", "bd-epic-1", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-task-2", "bd-epic-1", "parent-child", "tester")
            .unwrap();

        let epics = load_epic_statuses(&storage).unwrap();
        let epic_status = find_epic(&epics, "bd-epic-1").expect("epic not found");
        assert_eq!(epic_status.total_children, 2);
        assert_eq!(epic_status.closed_children, 0);
        assert!(!epic_status.eligible_for_close);

        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(Utc::now())),
            close_reason: Some(Some("Done".to_string())),
            ..Default::default()
        };
        storage
            .update_issue("bd-task-1", &update, "tester")
            .unwrap();

        let epics = load_epic_statuses(&storage).unwrap();
        let epic_status = find_epic(&epics, "bd-epic-1").expect("epic not found");
        assert_eq!(epic_status.total_children, 2);
        assert_eq!(epic_status.closed_children, 1);
        assert!(!epic_status.eligible_for_close);

        storage
            .update_issue("bd-task-2", &update, "tester")
            .unwrap();
        let epics = load_epic_statuses(&storage).unwrap();
        let epic_status = find_epic(&epics, "bd-epic-1").expect("epic not found");
        assert_eq!(epic_status.total_children, 2);
        assert_eq!(epic_status.closed_children, 2);
        assert!(epic_status.eligible_for_close);
    }

    #[test]
    fn epic_status_childless_epic_not_eligible() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let epic = base_issue("bd-epic-2", "Childless", IssueType::Epic, Status::Open);
        storage.create_issue(&epic, "tester").unwrap();

        let epics = load_epic_statuses(&storage).unwrap();
        let epic_status = find_epic(&epics, "bd-epic-2").expect("epic not found");
        assert_eq!(epic_status.total_children, 0);
        assert_eq!(epic_status.closed_children, 0);
        assert!(!epic_status.eligible_for_close);
    }

    #[test]
    fn close_eligible_epics_rolls_back_entire_batch_on_capacity_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for suffix in ["a", "b"] {
            let epic_id = format!("bd-epic-cap-{suffix}");
            let child_id = format!("bd-child-cap-{suffix}");
            storage
                .create_issue(
                    &base_issue(&epic_id, "Epic", IssueType::Epic, Status::Open),
                    "tester",
                )
                .unwrap();
            let mut child = base_issue(&child_id, "Child", IssueType::Task, Status::Closed);
            child.closed_at = Some(Utc::now());
            child.close_reason = Some("Done".to_string());
            storage.create_issue(&child, "tester").unwrap();
            storage
                .add_dependency(&child_id, &epic_id, "parent-child", "tester")
                .unwrap();
        }

        let epics: Vec<EpicStatus> = load_epic_statuses(&storage)
            .unwrap()
            .into_iter()
            .filter(|epic| epic.eligible_for_close)
            .collect();
        assert_eq!(epics.len(), 2);

        let mut policy = crate::close_policy::CapacityPolicy::default();
        policy.statuses.insert(
            "closed".to_string(),
            crate::close_policy::CapacityLimit {
                soft: None,
                hard: Some(3),
            },
        );
        storage.set_workflow_capacity_policy(policy);

        let error = close_eligible_epics_atomically(
            &mut storage,
            &epics,
            "tester",
            Utc::now(),
            "All children completed",
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::error::BeadsError::WorkflowCapacityExceeded { .. }
        ));
        for id in ["bd-epic-cap-a", "bd-epic-cap-b"] {
            assert_eq!(storage.get_issue(id).unwrap().unwrap().status, Status::Open);
        }

        let mut policy = crate::close_policy::CapacityPolicy::default();
        policy.statuses.insert(
            "closed".to_string(),
            crate::close_policy::CapacityLimit {
                soft: Some(3),
                hard: Some(5),
            },
        );
        storage.set_workflow_capacity_policy(policy);

        let closed = close_eligible_epics_atomically(
            &mut storage,
            &epics,
            "tester",
            Utc::now(),
            "All children completed",
            None,
        )
        .unwrap();
        assert_eq!(closed.len(), 2);

        let warnings = storage.take_capacity_warnings();
        assert_eq!(
            warnings.len(),
            1,
            "one final-state warning is emitted per affected capacity"
        );
        assert_eq!(warnings[0].capacity_kind, "status");
        assert_eq!(warnings[0].capacity_name, "closed");
        assert_eq!(warnings[0].current, 2);
        assert_eq!(warnings[0].prospective, 4);
        assert_eq!(warnings[0].soft_limit, 3);
    }

    #[test]
    fn epic_status_includes_deferred_epics() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let epic = base_issue(
            "bd-epic-deferred",
            "Deferred epic",
            IssueType::Epic,
            Status::Deferred,
        );
        storage.create_issue(&epic, "tester").unwrap();

        let epics = load_epic_statuses(&storage).unwrap();
        let epic_status = find_epic(&epics, "bd-epic-deferred").expect("deferred epic not found");
        assert_eq!(epic_status.total_children, 0);
        assert_eq!(epic_status.closed_children, 0);
        assert!(!epic_status.eligible_for_close);
    }

    #[test]
    fn epic_display_fields_sanitize_terminal_controls() {
        let id = format_epic_id("bd-epic\x1b]52;c;bad\x07", false);
        let title = format_epic_title("Title\x1b[2J\rreset", false);

        assert!(!id.contains('\x1b'));
        assert!(!id.contains('\x07'));
        assert!(!title.contains('\x1b'));
        assert!(!title.contains('\r'));
        assert_eq!(id, "bd-epic\\u{1b}]52;c;bad\\u{7}");
        assert_eq!(title, "Title\\u{1b}[2J\\rreset");
    }

    #[test]
    fn epic_completion_helpers_handle_zero_and_inconsistent_counts() {
        assert_eq!(completion_percentage(0, 0), 0);
        assert_eq!(completion_percentage(2, 4), 50);
        assert_eq!(completion_percentage(usize::MAX, 2), 100);
        assert_eq!(completion_percentage(usize::MAX, usize::MAX), 100);

        assert_eq!(completion_bar_filled_width(0, 0, 20), 0);
        assert_eq!(completion_bar_filled_width(1, 4, 20), 5);
        assert_eq!(completion_bar_filled_width(usize::MAX, 2, 20), 20);
        assert_eq!(completion_bar_filled_width(usize::MAX, usize::MAX, 20), 20);
    }

    #[test]
    fn epic_progress_bar_clamps_inconsistent_closed_counts() {
        let theme = crate::output::Theme::default();
        let mut content = Text::new("");

        render_progress_bar(&mut content, 3, 2, completion_percentage(3, 2), &theme);

        let rendered = content.plain();
        assert_eq!(rendered.matches('█').count(), 20);
        assert!(!rendered.contains('░'));
        assert!(rendered.contains("3/2 (100%)"));
    }
}
