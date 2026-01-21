//! Dependency command implementation.

use crate::cli::{
    DepAddArgs, DepCommands, DepCyclesArgs, DepDirection, DepListArgs, DepRemoveArgs, DepTreeArgs,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::truncate_title;
use crate::model::DependencyType;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

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
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let storage = &mut storage_ctx.storage;

    let actor = config::resolve_actor(&config_layer);

    let external_db_paths = config::external_project_db_paths(&config_layer, &beads_dir);

    match command {
        DepCommands::Add(args) => dep_add(args, storage, &resolver, &all_ids, &actor, json, ctx),
        DepCommands::Remove(args) => {
            dep_remove(args, storage, &resolver, &all_ids, &actor, json, ctx)
        }
        DepCommands::List(args) => dep_list(
            args,
            storage,
            &resolver,
            &all_ids,
            &external_db_paths,
            json,
            ctx,
        ),
        DepCommands::Tree(args) => dep_tree(
            args,
            storage,
            &resolver,
            &all_ids,
            &external_db_paths,
            json,
            ctx,
        ),
        DepCommands::Cycles(args) => dep_cycles(args, storage, json, ctx),
    }?;

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
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
    id: String,
    title: String,
    depth: usize,
    parent_id: Option<String>,
    priority: i32,
    status: String,
    truncated: bool,
}

/// JSON output for dep cycles
#[derive(Serialize)]
struct CyclesResult {
    cycles: Vec<Vec<String>>,
    count: usize,
}

fn dep_add(
    args: &DepAddArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, &args.issue)?;

    // External dependencies don't need resolution
    let depends_on_id = if args.depends_on.starts_with("external:") {
        args.depends_on.clone()
    } else {
        resolve_issue_id(storage, resolver, all_ids, &args.depends_on)?
    };

    // Parse and validate dependency type
    let dep_type_str = &args.dep_type;
    let dep_type: DependencyType = dep_type_str.parse().map_err(|_| BeadsError::Validation {
        field: "type".to_string(),
        reason: format!("Invalid dependency type: {dep_type_str}"),
    })?;

    // Disallow accidental custom types from typos
    if let DependencyType::Custom(_) = dep_type {
        // We enforce standard types for reliability unless it looks like a deliberate custom type
        // For now, let's strictly enforce known types to prevent typos like "parent_child"
        // which would otherwise be accepted as a non-blocking custom type.
        return Err(BeadsError::Validation {
            field: "type".to_string(),
            reason: format!(
                "Unknown dependency type: '{dep_type_str}'. \
                 Allowed types: blocks, parent-child, conditional-blocks, waits-for, \
                 related, discovered-from, replies-to, relates-to, duplicates, \
                 supersedes, caused-by"
            ),
        });
    }

    // Self-dependency check
    if issue_id == depends_on_id {
        return Err(BeadsError::SelfDependency { id: issue_id });
    }

    // Cycle check for blocking types only
    if dep_type.is_blocking()
        && !depends_on_id.starts_with("external:")
        && storage.would_create_cycle(&issue_id, &depends_on_id, true)?
    {
        return Err(BeadsError::DependencyCycle {
            path: format!("{issue_id} -> {depends_on_id}"),
        });
    }

    let added = storage.add_dependency(&issue_id, &depends_on_id, dep_type.as_str(), actor)?;

    if ctx.is_json() {
        let result = DepActionResult {
            status: if added { "ok" } else { "exists" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type: dep_type.as_str().to_string(),
            action: if added { "added" } else { "already_exists" }.to_string(),
        };
        ctx.json_pretty(&result);
    } else if added {
        if ctx.is_rich() {
            // Rich mode: Show detailed visual feedback
            ctx.success(&format!(
                "Added dependency: {} → {}",
                issue_id, depends_on_id
            ));
            let relationship = match dep_type {
                DependencyType::Blocks => format!("  {} now blocks {}", issue_id, depends_on_id),
                DependencyType::ParentChild => {
                    format!("  {} is parent of {}", depends_on_id, issue_id)
                }
                DependencyType::WaitsFor => {
                    format!("  {} waits for {}", issue_id, depends_on_id)
                }
                _ => format!("  Relationship: {}", dep_type.as_str()),
            };
            ctx.print(&relationship);
        } else {
            ctx.success(&format!(
                "Added dependency: {} -> {} ({})",
                issue_id,
                depends_on_id,
                dep_type.as_str()
            ));
        }
    } else {
        ctx.info(&format!(
            "Dependency already exists: {issue_id} → {depends_on_id}"
        ));
    }

    Ok(())
}

fn dep_remove(
    args: &DepRemoveArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, &args.issue)?;

    // External dependencies don't need resolution
    let depends_on_id = if args.depends_on.starts_with("external:") {
        args.depends_on.clone()
    } else {
        resolve_issue_id(storage, resolver, all_ids, &args.depends_on)?
    };

    let removed = storage.remove_dependency(&issue_id, &depends_on_id, actor)?;

    if ctx.is_json() {
        let result = DepActionResult {
            status: if removed { "ok" } else { "not_found" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type: "unknown".to_string(),
            action: if removed { "removed" } else { "not_found" }.to_string(),
        };
        ctx.json_pretty(&result);
    } else if removed {
        if ctx.is_rich() {
            ctx.success(&format!(
                "Removed dependency: {} → {}",
                issue_id, depends_on_id
            ));
            ctx.print(&format!(
                "  {} no longer depends on {}",
                issue_id, depends_on_id
            ));
        } else {
            ctx.success(&format!(
                "Removed dependency: {issue_id} -> {depends_on_id}"
            ));
        }
    } else {
        ctx.warning(&format!(
            "Dependency not found: {issue_id} → {depends_on_id}"
        ));
    }

    Ok(())
}

fn dep_list(
    args: &DepListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    external_db_paths: &HashMap<String, PathBuf>,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, &args.issue)?;

    let mut items = Vec::new();

    // Get dependencies (what this issue depends on)
    if matches!(args.direction, DepDirection::Down | DepDirection::Both) {
        let deps = storage.get_dependencies_with_metadata(&issue_id)?;
        for dep in deps {
            if let Some(ref filter_type) = args.dep_type {
                if dep.dep_type != *filter_type {
                    continue;
                }
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
            if let Some(ref filter_type) = args.dep_type {
                if dep.dep_type != *filter_type {
                    continue;
                }
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

    if ctx.is_json() {
        ctx.json_pretty(&items);
        return Ok(());
    }

    if items.is_empty() {
        let direction_str = match args.direction {
            DepDirection::Down => "dependencies",
            DepDirection::Up => "dependents",
            DepDirection::Both => "dependencies or dependents",
        };
        ctx.info(&format!("No {direction_str} for {issue_id}"));
        return Ok(());
    }

    if ctx.is_rich() {
        // Rich mode: Use panel with tree-like display
        render_dep_list_rich(ctx, &issue_id, &items, args.direction);
    } else {
        // Plain mode: Simple text output
        let header = match args.direction {
            DepDirection::Down => format!("Dependencies of {} ({}):", issue_id, items.len()),
            DepDirection::Up => format!("Dependents of {} ({}):", issue_id, items.len()),
            DepDirection::Both => format!(
                "Dependencies and dependents of {} ({}):",
                issue_id,
                items.len()
            ),
        };
        ctx.info(&header);

        for item in &items {
            let arrow = if item.issue_id == issue_id {
                format!("  -> {} ({})", item.depends_on_id, item.dep_type)
            } else {
                format!("  <- {} ({})", item.issue_id, item.dep_type)
            };
            ctx.print(&format!(
                "{}: {} [P{}] [{}]",
                arrow, item.title, item.priority, item.status
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

    let mut content = String::new();

    // Show dependencies (what this issue depends on)
    if !deps.is_empty() && matches!(direction, DepDirection::Down | DepDirection::Both) {
        content.push_str(&format!("[bold]Depends on ({}):[/]\n", deps.len()));
        for (i, item) in deps.iter().enumerate() {
            let prefix = if i == deps.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            let status_indicator = format_status_indicator(&item.status);
            content.push_str(&format!(
                "{} {} {} {}\n",
                prefix, item.depends_on_id, status_indicator, item.title
            ));
        }
    }

    // Add separator if showing both directions
    if !deps.is_empty() && !dependents.is_empty() && matches!(direction, DepDirection::Both) {
        content.push('\n');
    }

    // Show dependents (what depends on this issue)
    if !dependents.is_empty() && matches!(direction, DepDirection::Up | DepDirection::Both) {
        content.push_str(&format!(
            "[bold]Blocked by this ({}):[/]\n",
            dependents.len()
        ));
        for (i, item) in dependents.iter().enumerate() {
            let prefix = if i == dependents.len() - 1 {
                "└──"
            } else {
                "├──"
            };
            let status_indicator = format_status_indicator(&item.status);
            content.push_str(&format!(
                "{} {} {} {}\n",
                prefix, item.issue_id, status_indicator, item.title
            ));
        }
    }

    let panel = Panel::from_text(&content)
        .title(Text::new(format!("Dependencies for {}", issue_id)))
        .border_style(theme.panel_border.clone());

    ctx.render(&panel);
}

/// Format status indicator with appropriate styling hints
fn format_status_indicator(status: &str) -> String {
    match status {
        "open" => "[green][open][/]".to_string(),
        "in_progress" => "[yellow][in-progress][/]".to_string(),
        "closed" => "[dim][closed] ✓[/]".to_string(),
        "blocked" => "[red][blocked][/]".to_string(),
        _ => format!("[{}]", status),
    }
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

        if item.title.is_empty() {
            let prefix = if satisfied { "✓" } else { "⏳" };
            item.title = parse_external_dep_id(external_id).map_or_else(
                || format!("{prefix} {external_id}"),
                |(project, capability)| format!("{prefix} {project}:{capability}"),
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dep_tree(
    args: &DepTreeArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    external_db_paths: &HashMap<String, PathBuf>,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let root_id = resolve_issue_id(storage, resolver, all_ids, &args.issue)?;
    let root_issue = storage
        .get_issue(&root_id)?
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: root_id.clone(),
        })?;

    // Helper struct for BFS
    #[allow(clippy::items_after_statements)]
    struct QueueItem {
        id: String,
        depth: usize,
        parent_id: Option<String>,
        path: Vec<String>,
    }

    let external_statuses =
        storage.resolve_external_dependency_statuses(external_db_paths, false)?;

    let mut nodes = Vec::new();

    let mut queue = vec![QueueItem {
        id: root_id.clone(),
        depth: 0,
        parent_id: None,
        path: Vec::new(),
    }];

    while let Some(item) = queue.pop() {
        // Cycle detection: check if current ID is already in the path
        if item.path.contains(&item.id) {
            continue;
        }

        let issue = if item.id == root_id {
            Some(root_issue.clone())
        } else if item.id.starts_with("external:") {
            None
        } else {
            storage.get_issue(&item.id)?
        };

        let (title, priority, status) = if let Some(ref issue) = issue {
            (
                issue.title.clone(),
                issue.priority.0,
                issue.status.as_str().to_string(),
            )
        } else if item.id.starts_with("external:") {
            let satisfied = external_statuses.get(&item.id).copied().unwrap_or(false);
            let status = if satisfied { "closed" } else { "blocked" };
            let prefix = if satisfied { "✓" } else { "⏳" };
            let title = if let Some((project, capability)) = parse_external_dep_id(&item.id) {
                format!("{prefix} {project}:{capability}")
            } else {
                format!("{prefix} {}", item.id)
            };
            (title, 2, status.to_string())
        } else {
            // Missing issue
            (item.id.clone(), 2, "unknown".to_string())
        };

        let truncated = item.depth >= args.max_depth;

        nodes.push(TreeNode {
            id: item.id.clone(),
            title,
            depth: item.depth,
            parent_id: item.parent_id.clone(),
            priority,
            status,
            truncated,
        });

        // Don't expand if at max depth
        if item.depth < args.max_depth && !item.id.starts_with("external:") {
            let mut new_path = item.path.clone();
            new_path.push(item.id.clone());

            // Get dependencies (issues that this one depends on)
            let mut dependencies = storage.get_dependencies(&item.id)?;

            // Get full issue details for sorting
            // This is slightly inefficient (N queries), but necessary for sorting by priority.
            // Optimization: fetch all at once or accept ID sort.
            // For now, let's sort by ID to be deterministic, or fetch details.
            // The original code sorted the FINAL list.
            // To maintain DFS order with sorted siblings, we must sort here.

            // Let's just sort by ID for stability and speed, priority sorting would require fetching issues.
            dependencies.sort();
            // Push in reverse order so first item pops first
            for dep_id in dependencies.into_iter().rev() {
                // No global visited check here
                queue.push(QueueItem {
                    id: dep_id,
                    depth: item.depth + 1,
                    parent_id: Some(item.id.clone()),
                    path: new_path.clone(),
                });
            }
        }
    }

    if ctx.is_json() {
        ctx.json_pretty(&nodes);
        return Ok(());
    }

    // Mermaid format output
    if args.format.eq_ignore_ascii_case("mermaid") {
        // Use println! directly to avoid rich_rust markup interpretation
        println!("graph TD");
        // Output node definitions
        for node in &nodes {
            // Escape quotes in title for mermaid
            let escaped_title = node.title.replace('"', "'");
            println!(
                "    {}[\"{}: {} [P{}]\"]",
                node.id, node.id, escaped_title, node.priority
            );
        }
        // Output edges (parent --> child shows dependency direction)
        for node in &nodes {
            if let Some(ref parent_id) = node.parent_id {
                // parent_id depends on node.id, so show parent_id --> node.id
                println!("    {} --> {}", parent_id, node.id);
            }
        }
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
            } else {
                "├── "
            };
            ctx.print(&format!(
                "{}{}{}: {} [P{}] [{}]",
                indent, prefix, node.id, node.title, node.priority, node.status
            ));
        }
    }

    Ok(())
}

/// Render dependency tree in rich mode using Tree component
fn render_dep_tree_rich(ctx: &OutputContext, nodes: &[TreeNode]) {
    if nodes.is_empty() {
        return;
    }

    let theme = ctx.theme();

    // Build tree structure from flat nodes list
    // The nodes are in DFS order with parent_id references
    let root = build_tree_node_rich(&nodes[0], nodes);
    let tree = Tree::new(root)
        .guides(TreeGuides::Rounded)
        .guide_style(theme.dimmed.clone());

    ctx.render(&tree);
}

/// Recursively build a tree node for rich rendering
fn build_tree_node_rich(
    node: &TreeNode,
    all_nodes: &[TreeNode],
) -> rich_rust::renderables::TreeNode {
    // Format the node label with status styling
    let status_style = match node.status.as_str() {
        "open" => "[green]",
        "in_progress" => "[yellow]",
        "closed" => "[dim]",
        "blocked" => "[red]",
        _ => "[white]",
    };
    let status_close = "[/]";

    let status_indicator = match node.status.as_str() {
        "closed" => " ✓",
        "blocked" => " ⚠",
        _ => "",
    };

    let label = if node.truncated {
        format!(
            "{} {}[{}]{}{} {} [dim](truncated)[/]",
            node.id,
            status_style,
            node.status,
            status_close,
            status_indicator,
            truncate_title(&node.title, 35)
        )
    } else {
        format!(
            "{} {}[{}]{}{} {}",
            node.id,
            status_style,
            node.status,
            status_close,
            status_indicator,
            truncate_title(&node.title, 40)
        )
    };

    let mut tree_node = rich_rust::renderables::TreeNode::new(Text::new(label));

    // Find and add children (nodes whose parent_id matches this node's id)
    for child in all_nodes
        .iter()
        .filter(|n| n.parent_id.as_ref() == Some(&node.id))
    {
        let child_node = build_tree_node_rich(child, all_nodes);
        tree_node = tree_node.child(child_node);
    }

    tree_node
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
    _args: &DepCyclesArgs,
    storage: &SqliteStorage,
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let cycles = storage.detect_all_cycles()?;
    let count = cycles.len();

    if ctx.is_json() {
        let result = CyclesResult { cycles, count };
        ctx.json_pretty(&result);
        return Ok(());
    }

    if count == 0 {
        ctx.success("No dependency cycles detected.");
    } else if ctx.is_rich() {
        // Rich mode: Show cycles with red highlighting in a panel
        render_cycles_rich(ctx, &cycles, count);
    } else {
        // Plain mode: Simple text output
        ctx.warning(&format!("Found {count} dependency cycle(s):"));
        for (i, cycle) in cycles.iter().enumerate() {
            ctx.print(&format!("  {}. {}", i + 1, cycle.join(" -> ")));
        }
    }

    Ok(())
}

/// Render cycles in rich mode with red highlighting
fn render_cycles_rich(ctx: &OutputContext, cycles: &[Vec<String>], count: usize) {
    let theme = ctx.theme();

    let mut content = String::new();
    content.push_str(&format!(
        "[bold red]⚠ {} dependency cycle(s) detected:[/]\n\n",
        count
    ));

    for (i, cycle) in cycles.iter().enumerate() {
        // Format cycle path with arrows
        let cycle_path = cycle.join(" [red]→[/] ");
        content.push_str(&format!("[bold]Cycle {}:[/]\n", i + 1));
        content.push_str(&format!("  [red]{}[/]\n", cycle_path));

        // Add underline visual
        let path_len = cycle.iter().map(|s| s.len() + 4).sum::<usize>();
        content.push_str(&format!("  [red]{}[/]\n", "^".repeat(path_len.min(60))));

        if i < cycles.len() - 1 {
            content.push('\n');
        }
    }

    content.push_str("\n[dim]Suggestion: Remove one dependency from each cycle to break it.[/]");

    let panel = Panel::from_text(&content)
        .title(Text::new("Dependency Cycles"))
        .border_style(theme.error.clone());

    ctx.render(&panel);
}

fn resolve_issue_id(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    input: &str,
) -> Result<String> {
    resolver
        .resolve(
            input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| find_matching_ids(all_ids, hash),
        )
        .map(|resolved| resolved.id)
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
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"count\":2"));
        assert!(json.contains("bd-001"));
        info!("test_cycles_result_json: assertions passed");
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
    fn test_dep_direction_variants() {
        init_test_logging();
        info!("test_dep_direction_variants: starting");
        assert!(matches!(DepDirection::Down, DepDirection::Down));
        assert!(matches!(DepDirection::Up, DepDirection::Up));
        assert!(matches!(DepDirection::Both, DepDirection::Both));
        info!("test_dep_direction_variants: assertions passed");
    }
}
