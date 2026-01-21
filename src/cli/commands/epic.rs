//! Epic command implementation.

use crate::cli::{EpicCloseEligibleArgs, EpicCommands, EpicStatusArgs};
use crate::config;
use crate::error::Result;
use crate::model::{EpicStatus, IssueType, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{IssueUpdate, ListFilters, SqliteStorage};
use chrono::Utc;
use crossterm::style::Stylize;
use rich_rust::prelude::*;
use serde::Serialize;
use std::cmp::Ordering;

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

fn execute_status(
    args: &EpicStatusArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;
    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let use_color = config::should_use_color(&config_layer);

    let mut epics = load_epic_statuses(storage)?;
    if args.eligible_only {
        epics.retain(|e| e.eligible_for_close);
    }

    if ctx.is_json() {
        ctx.json_pretty(&epics);
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
}

fn execute_close_eligible(
    args: &EpicCloseEligibleArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);

    let storage = &mut storage_ctx.storage;
    let mut epics = load_epic_statuses(storage)?;
    epics.retain(|e| e.eligible_for_close);

    if epics.is_empty() {
        if ctx.is_json() {
            ctx.json(&Vec::<EpicStatus>::new());
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_eligible_rich(ctx);
        } else {
            println!("No epics eligible for closure");
        }
        return Ok(());
    }

    if args.dry_run {
        if ctx.is_json() {
            ctx.json_pretty(&epics);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_rich(&epics, ctx);
        } else {
            println!("Would close {} epic(s):", epics.len());
            for epic_status in &epics {
                println!("  - {}: {}", epic_status.epic.id, epic_status.epic.title);
            }
        }
        return Ok(());
    }

    let mut closed_ids = Vec::new();
    for epic_status in &epics {
        let now = Utc::now();
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: Some(Some("All children completed".to_string())),
            ..Default::default()
        };

        match storage.update_issue(&epic_status.epic.id, &update, &actor) {
            Ok(_) => closed_ids.push(epic_status.epic.id.clone()),
            Err(err) => eprintln!("Error closing {}: {err}", epic_status.epic.id),
        }
    }

    if !closed_ids.is_empty() {
        storage.rebuild_blocked_cache(true)?;
    }

    if ctx.is_json() {
        let result = CloseEligibleResult {
            closed: closed_ids.clone(),
            count: closed_ids.len(),
        };
        ctx.json_pretty(&result);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_close_result_rich(&closed_ids, ctx);
    } else {
        println!("✓ Closed {} epic(s)", closed_ids.len());
        for id in &closed_ids {
            println!("  - {id}");
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

fn load_epic_statuses(storage: &SqliteStorage) -> Result<Vec<EpicStatus>> {
    let filters = ListFilters {
        types: Some(vec![IssueType::Epic]),
        include_closed: false,
        ..Default::default()
    };
    let epics = storage.list_issues(&filters)?;

    let mut statuses = Vec::new();
    for epic in epics {
        let children = storage.get_dependents_with_metadata(&epic.id)?;
        let parent_children: Vec<_> = children
            .into_iter()
            .filter(|c| c.dep_type == "parent-child")
            .collect();
        let total_children = parent_children.len();
        let closed_children = parent_children
            .iter()
            .filter(|c| matches!(c.status, Status::Closed | Status::Tombstone))
            .count();
        let eligible_for_close = total_children > 0 && closed_children == total_children;

        statuses.push(EpicStatus {
            epic,
            total_children,
            closed_children,
            eligible_for_close,
        });
    }

    statuses.sort_by(|a, b| {
        let primary = a.epic.priority.cmp(&b.epic.priority);
        if primary == Ordering::Equal {
            a.epic.created_at.cmp(&b.epic.created_at)
        } else {
            primary
        }
    });

    Ok(statuses)
}

fn render_epic_status(epic_status: &EpicStatus, use_color: bool) {
    let total = epic_status.total_children;
    let closed = epic_status.closed_children;
    let percentage = if total > 0 { (closed * 100) / total } else { 0 };
    let status_icon = render_status_icon(epic_status.eligible_for_close, percentage, use_color);

    let id = if use_color {
        epic_status.epic.id.clone().cyan().to_string()
    } else {
        epic_status.epic.id.clone()
    };

    let title = if use_color {
        epic_status.epic.title.clone().bold().to_string()
    } else {
        epic_status.epic.title.clone()
    };

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
        let percentage = if total > 0 { (closed * 100) / total } else { 0 };

        // Status icon
        if epic_status.eligible_for_close {
            content.append_styled("✓ ", theme.success.clone());
        } else if percentage > 0 {
            content.append_styled("○ ", theme.warning.clone());
        } else {
            content.append_styled("○ ", theme.dimmed.clone());
        }

        // ID and title
        content.append_styled(&epic_status.epic.id, theme.issue_id.clone());
        content.append(" ");
        content.append_styled(&epic_status.epic.title, theme.emphasis.clone());
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
    let filled = if total > 0 {
        (closed * bar_width) / total
    } else {
        0
    };
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
        content.append_styled(&epic_status.epic.id, theme.issue_id.clone());
        content.append(" ");
        content.append(&epic_status.epic.title);
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
            content.append_styled(id, theme.issue_id.clone());
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
    use crate::model::{Issue, Priority};
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
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
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
}
