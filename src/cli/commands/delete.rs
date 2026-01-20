//! Delete command implementation.
//!
//! Creates tombstones for issues, handles dependencies, and supports
//! cascade/force/dry-run modes.

use crate::cli::DeleteArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

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
        return Err(BeadsError::validation("ids", "no issue IDs provided"));
    }

    // Deduplicate
    let ids: Vec<String> = ids
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // 2. Open storage
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let storage = &mut storage_ctx.storage;

    // 3. Validate all IDs exist
    for id in &ids {
        if storage.get_issue(id)?.is_none() {
            return Err(BeadsError::IssueNotFound { id: id.clone() });
        }
    }

    // 4. Check for dependents (if not --force and not --cascade)
    let delete_set: HashSet<String> = ids.iter().cloned().collect();
    let mut all_dependents: Vec<String> = Vec::new();

    for id in &ids {
        let dependents = storage.get_dependents(id)?;
        for dep_id in dependents {
            if !delete_set.contains(&dep_id) {
                all_dependents.push(dep_id);
            }
        }
    }
    all_dependents.sort();
    all_dependents.dedup();

    if !all_dependents.is_empty() && !args.force && !args.cascade {
        // Preview mode: show what would happen
        if ctx.is_rich() {
            render_dependents_warning_rich(&all_dependents, storage, ctx);
        } else {
            println!("The following issues depend on issues being deleted:");
            for dep in &all_dependents {
                println!("  - {dep}");
            }
            println!();
            println!(
                "Use --force to orphan these dependents, or --cascade to delete them recursively."
            );
            println!("No changes made (preview mode).");
        }
        return Ok(());
    }

    // 5. Dry-run mode
    if args.dry_run {
        if ctx.is_rich() {
            let cascade_ids: Vec<String> = if args.cascade {
                all_dependents.clone()
            } else {
                vec![]
            };
            let orphan_ids: Vec<String> = if args.force && !args.cascade {
                all_dependents.clone()
            } else {
                vec![]
            };
            render_dry_run_rich(&ids, &cascade_ids, &orphan_ids, storage, ctx);
        } else {
            println!("Dry-run: Would delete {} issue(s):", ids.len());
            for id in &ids {
                let issue = storage
                    .get_issue(id)?
                    .ok_or_else(|| BeadsError::IssueNotFound { id: id.clone() })?;
                println!("  - {}: {}", id, issue.title);
            }
            if args.cascade && !all_dependents.is_empty() {
                println!(
                    "Would also cascade delete {} dependent(s):",
                    all_dependents.len()
                );
                for dep in &all_dependents {
                    println!("  - {dep}");
                }
            }
            if args.force && !all_dependents.is_empty() {
                println!("Would orphan {} dependent(s):", all_dependents.len());
                for dep in &all_dependents {
                    println!("  - {dep}");
                }
            }
        }
        return Ok(());
    }

    // 6. Build final delete set
    let mut final_delete_set: HashSet<String> = delete_set;
    if args.cascade {
        // Recursively collect all dependents
        let cascade_ids = collect_cascade_dependents(storage, &ids)?;
        final_delete_set.extend(cascade_ids);
    }

    // 7. Get actor
    let actor = config::resolve_actor(&config_layer);

    // 8. Perform deletion
    let mut result = DeleteResult::new();

    // First, remove all dependency links for issues being deleted
    for id in &final_delete_set {
        let deps_removed = storage.remove_all_dependencies(id, &actor)?;
        result.dependencies_removed += deps_removed;
    }

    // Track orphaned issues (only relevant for --force mode)
    if args.force && !args.cascade {
        result.orphaned_issues.clone_from(&all_dependents);
    }

    // Delete each issue (create tombstone)
    let final_ids: Vec<String> = final_delete_set.into_iter().collect();
    for id in &final_ids {
        storage.delete_issue(id, &actor, &args.reason, None)?;
        result.deleted.push(id.clone());
    }
    result.deleted_count = result.deleted.len();

    // 9. Output
    if ctx.is_json() {
        ctx.json_pretty(&result);
        storage_ctx.flush_no_db_if_dirty()?;
        return Ok(());
    }

    result.deleted.sort();

    if ctx.is_rich() {
        render_delete_result_rich(&result, storage, ctx);
    } else {
        println!("Deleted {} issue(s):", result.deleted_count);
        for id in &result.deleted {
            println!("  - {id}");
        }

        if result.dependencies_removed > 0 {
            println!("Removed {} dependency link(s)", result.dependencies_removed);
        }

        if !result.orphaned_issues.is_empty() {
            println!("Orphaned {} issue(s):", result.orphaned_issues.len());
            for id in &result.orphaned_issues {
                println!("  - {id}");
            }
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
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

/// Recursively collect all dependents for cascade deletion.
fn collect_cascade_dependents(
    storage: &SqliteStorage,
    initial_ids: &[String],
) -> Result<HashSet<String>> {
    let mut all_ids: HashSet<String> = initial_ids.iter().cloned().collect();
    let mut to_process: Vec<String> = initial_ids.to_vec();

    while let Some(id) = to_process.pop() {
        let dependents = storage.get_dependents(&id)?;
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

/// Render the dependents warning panel in rich format.
fn render_dependents_warning_rich(
    dependents: &[String],
    storage: &SqliteStorage,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled(
        "The following issues depend on issues being deleted:\n\n",
        theme.warning.clone(),
    );

    for dep_id in dependents {
        content.append_styled("  \u{2022} ", theme.dimmed.clone());
        content.append_styled(dep_id, theme.issue_id.clone());
        // Try to get title
        if let Ok(Some(issue)) = storage.get_issue(dep_id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(&issue.title);
        }
        content.append("\n");
    }

    content.append("\n");
    content.append_styled(
        "Use --force to orphan these dependents, or --cascade to delete them recursively.\n",
        theme.dimmed.clone(),
    );
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

    let mut content = Text::new("");

    // Main issues to delete
    content.append_styled("Would delete ", theme.dimmed.clone());
    content.append_styled(&format!("{}", ids.len()), theme.emphasis.clone());
    content.append_styled(" issue(s):\n\n", theme.dimmed.clone());

    for id in ids {
        content.append_styled("  \u{2717} ", theme.error.clone());
        content.append_styled(id, theme.issue_id.clone());
        if let Ok(Some(issue)) = storage.get_issue(id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(&issue.title);
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
            content.append_styled(id, theme.issue_id.clone());
            if let Ok(Some(issue)) = storage.get_issue(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(&issue.title);
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
            content.append_styled(id, theme.issue_id.clone());
            if let Ok(Some(issue)) = storage.get_issue(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(&issue.title);
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

    let mut content = Text::new("");

    // Deleted items
    content.append_styled("Deleted ", theme.success.clone());
    content.append_styled(&format!("{}", result.deleted_count), theme.emphasis.clone());
    content.append_styled(" issue(s):\n\n", theme.success.clone());

    for id in &result.deleted {
        content.append_styled("  \u{2713} ", theme.success.clone());
        content.append_styled(id, theme.issue_id.clone());
        if let Ok(Some(issue)) = storage.get_issue(id) {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(&issue.title);
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
            content.append_styled(id, theme.issue_id.clone());
            if let Ok(Some(issue)) = storage.get_issue(id) {
                content.append_styled(": ", theme.dimmed.clone());
                content.append(&issue.title);
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
                tx.execute(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    ["bd-b", "bd-a", "blocks", &Utc::now().to_rfc3339()],
                )?;
                tx.execute(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at) VALUES (?, ?, ?, ?)",
                    ["bd-c", "bd-b", "blocks", &Utc::now().to_rfc3339()],
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
}
