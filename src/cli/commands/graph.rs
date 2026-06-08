//! Graph command implementation.
//!
//! Visualizes dependency graphs with focus on reverse dependencies (dependents).
//!
//! - `br graph <issue-id>`: Show all dependents of an issue (what depends on it)
//! - `br graph --all`: Show connected components for `open`/`in_progress`/`blocked`/`deferred` issues

use crate::cli::GraphArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{DependencyType, Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids, split_prefix_remainder};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use tracing::debug;

/// JSON output for a single node in the graph.
#[derive(Debug, Clone, Serialize)]
struct GraphNode {
    id: String,
    title: String,
    status: String,
    priority: i32,
    depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
}

/// JSON output for the graph command (single issue mode).
#[derive(Debug, Serialize)]
struct SingleGraphOutput {
    root: String,
    nodes: Vec<GraphNode>,
    edges: Vec<(String, String)>,
    count: usize,
}

/// JSON output for connected component.
#[derive(Debug, Serialize)]
struct ConnectedComponent {
    nodes: Vec<GraphNode>,
    edges: Vec<(String, String)>,
    roots: Vec<String>,
}

/// JSON output for --all mode.
#[derive(Debug, Serialize)]
struct AllGraphOutput {
    components: Vec<ConnectedComponent>,
    total_nodes: usize,
    total_components: usize,
}

/// Execute the graph command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
/// Render the same `--all` text view that `bd graph --all` prints,
/// but into a String. Used by the TUI's Graph view-mode so it doesn't
/// have to redirect stdout.
///
/// # Errors
///
/// Returns an error if storage open or graph computation fails.
pub(crate) fn render_all_string(
    cli: &config::CliOverrides,
    args: &GraphArgs,
) -> Result<String> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    render_all_components(&storage_ctx.storage, args)
}

/// Build the component list and dispatch to the right text renderer.
/// Extracted from `graph_all` so the TUI path can reuse the same
/// data-fetch + formatting without going through `ctx`.
fn render_all_components(storage: &SqliteStorage, args: &GraphArgs) -> Result<String> {
    let components = compute_components(storage)?;
    let total_nodes: usize = components.iter().map(|c| c.nodes.len()).sum();
    let text = if args.no_cluster {
        render_all_flat_str(&components, total_nodes, args)
    } else {
        render_all_clustered_str(&components, total_nodes, args)
    };
    Ok(text)
}

pub fn execute(args: &GraphArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;

    if args.all {
        graph_all(&storage_ctx.storage, args, ctx)
    } else {
        let issue_id = args.issue.as_ref().ok_or_else(|| {
            BeadsError::validation("issue", "Issue ID required unless --all is specified")
        })?;

        let resolved_id = resolve_issue_id(&storage_ctx.storage, &resolver, &all_ids, issue_id)?;
        graph_single(&storage_ctx.storage, &resolved_id, args.compact, ctx)
    }
}

/// Show graph for a single issue (traverse dependents only).
fn graph_single(
    storage: &SqliteStorage,
    root_id: &str,
    compact: bool,
    ctx: &OutputContext,
) -> Result<()> {
    // Verify the root issue exists
    let root_issue = storage
        .get_issue(root_id)?
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: root_id.to_string(),
        })?;

    // DFS to find all dependents (reverse deps)
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();

    // Start with root
    stack.push((root_id.to_string(), 0));
    visited.insert(root_id.to_string());

    while let Some((current_id, depth)) = stack.pop() {
        let issue = if current_id == root_id {
            root_issue.clone()
        } else {
            storage.get_issue(&current_id)?.unwrap_or_else(|| {
                let mut i = root_issue.clone();
                i.id.clone_from(&current_id);
                i.title = "Unknown".to_string();
                i
            })
        };

        nodes.push(GraphNode {
            id: current_id.clone(),
            title: issue.title.clone(),
            status: issue.status.as_str().to_string(),
            priority: issue.priority.0,
            depth,
            sender: issue.sender.clone(),
        });

        // Get dependents (issues that depend on current_id)
        let mut dependents = storage.get_dependents_with_metadata(&current_id)?;

        // Only include dependency types that affect ready work
        dependents.retain(|dep| {
            dep.dep_type
                .parse::<DependencyType>()
                .unwrap_or(DependencyType::Blocks)
                .affects_ready_work()
        });

        // Sort dependents to ensure deterministic DFS order (stack reverses order)
        dependents.sort_by(|a, b| a.priority.0.cmp(&b.priority.0).then(a.id.cmp(&b.id)));

        for dep in dependents.into_iter().rev() {
            // Record edge: dependent -> current (dependent depends on current)
            edges.push((dep.id.clone(), current_id.clone()));

            if !visited.contains(&dep.id) {
                visited.insert(dep.id.clone());
                stack.push((dep.id.clone(), depth + 1));
            }
        }
    }

    if ctx.is_json() {
        let output = SingleGraphOutput {
            root: root_id.to_string(),
            count: nodes.len(),
            nodes,
            edges,
        };
        ctx.json_pretty(&output);
        return Ok(());
    }

    // Text output
    if nodes.len() == 1 {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_dependents_rich(root_id, &root_issue, ctx);
        } else {
            println!("No dependents for {root_id}");
        }
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_single_graph_rich(&nodes, &root_issue, ctx);
    } else if compact {
        // One-liner format: root <- dep1 <- dep2 ...
        let dependent_ids: Vec<&str> = nodes.iter().skip(1).map(|n| n.id.as_str()).collect();
        println!("{} <- {}", root_id, dependent_ids.join(" <- "));
    } else {
        // Tree-like format
        println!("Dependents of {} ({} total):", root_id, nodes.len() - 1);
        println!();
        println!(
            "  {} [P{}] [{}] (root)",
            root_issue.title,
            root_issue.priority.0,
            root_issue.status.as_str()
        );

        for node in nodes.iter().skip(1) {
            let indent = "  ".repeat(node.depth + 1);
            println!(
                "{}← {}: {} [P{}] [{}]",
                indent, node.id, node.title, node.priority, node.status
            );
        }
    }

    Ok(())
}

/// Show graph for all `open`/`in_progress`/`blocked`/`deferred` issues.
#[allow(clippy::too_many_lines)]
/// Compute the connected-component view of the open/in-progress/
/// blocked/deferred issue graph. Shared by the stdout path and the
/// TUI graph view. Empty result is valid (no issues found).
fn compute_components(storage: &SqliteStorage) -> Result<Vec<ConnectedComponent>> {
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress, Status::Blocked, Status::Deferred]),
        include_closed: false,
        include_templates: false,
        ..Default::default()
    };

    let issues = storage.list_issues(&filters)?;
    if issues.is_empty() {
        return Ok(Vec::new());
    }

    let issue_set: HashSet<String> = issues.iter().map(|i| i.id.clone()).collect();
    let issue_map: HashMap<String, &crate::model::Issue> =
        issues.iter().map(|i| (i.id.clone(), i)).collect();

    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut blocking_edges: Vec<(String, String)> = Vec::new();
    let all_dependencies = storage.get_all_dependency_records()?;

    for issue in &issues {
        adj.entry(issue.id.clone()).or_default();
        if let Some(deps) = all_dependencies.get(&issue.id) {
            for dep in deps {
                if !dep.dep_type.affects_ready_work() {
                    continue;
                }
                let dep_id = &dep.depends_on_id;
                if issue_set.contains(dep_id) {
                    adj.entry(issue.id.clone()).or_default().push(dep_id.clone());
                    adj.entry(dep_id.clone()).or_default().push(issue.id.clone());
                    blocking_edges.push((issue.id.clone(), dep_id.clone()));
                }
            }
        }
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut components: Vec<ConnectedComponent> = Vec::new();

    for issue in &issues {
        if visited.contains(&issue.id) {
            continue;
        }

        let mut component_nodes: Vec<String> = Vec::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(issue.id.clone());
        visited.insert(issue.id.clone());

        while let Some(current) = queue.pop_front() {
            component_nodes.push(current.clone());
            if let Some(neighbors) = adj.get(&current) {
                for neighbor in neighbors {
                    if !visited.contains(neighbor) {
                        visited.insert(neighbor.clone());
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }

        let component_set: HashSet<&String> = component_nodes.iter().collect();
        let mut depths = calculate_depths(&all_dependencies, &component_nodes, &component_set);

        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut roots: Vec<String> = Vec::new();
        for node_id in &component_nodes {
            if let Some(issue) = issue_map.get(node_id) {
                let depth = depths.remove(node_id).unwrap_or(0);
                if depth == 0 {
                    roots.push(node_id.clone());
                }
                nodes.push(GraphNode {
                    id: node_id.clone(),
                    title: issue.title.clone(),
                    status: issue.status.as_str().to_string(),
                    priority: issue.priority.0,
                    depth,
                    sender: issue.sender.clone(),
                });
            }
        }

        nodes.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then(a.priority.cmp(&b.priority))
                .then(a.id.cmp(&b.id))
        });
        roots.sort();

        let component_edges: Vec<(String, String)> = blocking_edges
            .iter()
            .filter(|(from, to)| component_set.contains(from) && component_set.contains(to))
            .cloned()
            .collect();

        components.push(ConnectedComponent {
            nodes,
            edges: component_edges,
            roots,
        });
    }

    components.sort_by_key(|b| std::cmp::Reverse(b.nodes.len()));
    Ok(components)
}

fn graph_all(storage: &SqliteStorage, args: &GraphArgs, ctx: &OutputContext) -> Result<()> {
    let components = compute_components(storage)?;
    debug!(count = components.len(), "Computed components for graph");

    if components.is_empty() {
        if ctx.is_json() {
            let output = AllGraphOutput {
                components: vec![],
                total_nodes: 0,
                total_components: 0,
            };
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_issues_rich(ctx);
        } else {
            println!("No open/in_progress/blocked/deferred issues found");
        }
        return Ok(());
    }

    let total_nodes: usize = components.iter().map(|c| c.nodes.len()).sum();

    if ctx.is_json() {
        let output = AllGraphOutput {
            total_nodes,
            total_components: components.len(),
            components,
        };
        ctx.json_pretty(&output);
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_all_graph_rich_with_args(&components, total_nodes, args, ctx);
    } else if args.no_cluster {
        render_all_flat(&components, total_nodes, args);
    } else {
        render_all_clustered(&components, total_nodes, args);
    }

    Ok(())
}

/// Determine the "dominant prefix" of a component: most common prefix among its
/// nodes. Ties broken alphabetically. Mixed-prefix components return ("<mixed>",
/// vec of distinct prefixes).
fn component_prefix(component: &ConnectedComponent) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for node in &component.nodes {
        let prefix = split_prefix_remainder(&node.id)
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| "(no-prefix)".to_string());
        *counts.entry(prefix).or_insert(0) += 1;
    }
    if counts.len() > 1 {
        return "<cross-prefix>".to_string();
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(p, _)| p)
        .unwrap_or_else(|| "(no-prefix)".to_string())
}

fn terminal_cols() -> usize {
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols > 0 => cols as usize,
        _ => 100,
    }
}

/// Compute usable title width given the row prefix (indent + id + badges).
fn title_budget(args: &GraphArgs, used: usize) -> usize {
    let cols = terminal_cols();
    let cap = args.max_title.unwrap_or_else(|| cols.saturating_sub(used).max(40));
    cap
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    // Try to break on a word boundary.
    let limit = max_chars.saturating_sub(1);
    let prefix: String = s.chars().take(limit).collect();
    let trimmed = match prefix.rfind(char::is_whitespace) {
        Some(idx) if idx >= max_chars / 2 => prefix[..idx].trim_end().to_string(),
        _ => prefix.trim_end().to_string(),
    };
    format!("{trimmed}…")
}

/// Cluster components by dominant prefix and render with section headers.
fn render_all_clustered(components: &[ConnectedComponent], total_nodes: usize, args: &GraphArgs) {
    print!("{}", render_all_clustered_str(components, total_nodes, args));
}

fn render_all_clustered_str(
    components: &[ConnectedComponent],
    total_nodes: usize,
    args: &GraphArgs,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    if components.is_empty() {
        let _ = writeln!(out, "(no beads)");
        return out;
    }

    let _ = writeln!(
        out,
        "Dependency graph: {} issues in {} component(s)",
        total_nodes,
        components.len()
    );

    let mut groups: BTreeMap<String, Vec<&ConnectedComponent>> = BTreeMap::new();
    for component in components {
        groups
            .entry(component_prefix(component))
            .or_default()
            .push(component);
    }

    let mut prefixes: Vec<String> = groups
        .keys()
        .filter(|p| *p != "<cross-prefix>")
        .cloned()
        .collect();
    prefixes.sort();
    if groups.contains_key("<cross-prefix>") {
        prefixes.push("<cross-prefix>".to_string());
    }

    for prefix in &prefixes {
        let comps = groups.get(prefix).expect("present");
        let total_in_prefix: usize = comps.iter().map(|c| c.nodes.len()).sum();
        let component_word = if comps.len() == 1 { "component" } else { "components" };
        let _ = writeln!(
            out,
            "=== {prefix} ({n} {issue_word}, {c} {comp_word}) ===",
            n = total_in_prefix,
            issue_word = if total_in_prefix == 1 { "issue" } else { "issues" },
            c = comps.len(),
            comp_word = component_word,
        );

        for component in comps {
            out.push_str(&render_component_text_str(component, args, true));
        }
    }
    out
}

/// Flat (non-clustered) view — preserves backwards compatibility via --no-cluster.
fn render_all_flat(components: &[ConnectedComponent], total_nodes: usize, args: &GraphArgs) {
    print!("{}", render_all_flat_str(components, total_nodes, args));
}

fn render_all_flat_str(
    components: &[ConnectedComponent],
    total_nodes: usize,
    args: &GraphArgs,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Dependency graph: {} issues in {} component(s)",
        total_nodes,
        components.len()
    );
    let _ = writeln!(out);
    for (i, component) in components.iter().enumerate() {
        if args.compact {
            let ids: Vec<&str> = component.nodes.iter().map(|n| n.id.as_str()).collect();
            let _ = writeln!(out, "Component {}: {}", i + 1, ids.join(", "));
        } else {
            let _ = writeln!(
                out,
                "Component {} ({} issues, roots: {}):",
                i + 1,
                component.nodes.len(),
                component.roots.join(", ")
            );
            out.push_str(&render_component_body_str(component, args, "  "));
            let _ = writeln!(out);
        }
    }
    out
}

fn render_component_text_str(component: &ConnectedComponent, args: &GraphArgs, indent: bool) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    if args.compact {
        let ids: Vec<&str> = component.nodes.iter().map(|n| n.id.as_str()).collect();
        let prefix = if indent { "  " } else { "" };
        let _ = writeln!(out, "{prefix}{}", ids.join(" → "));
        return out;
    }

    if indent && component.nodes.len() > 1 {
        let _ = writeln!(
            out,
            "  ── component ({n} {issue_word}, roots: {roots})",
            n = component.nodes.len(),
            issue_word = if component.nodes.len() == 1 { "issue" } else { "issues" },
            roots = component.roots.join(", ")
        );
    }
    let base_indent = if indent { "    " } else { "  " };
    out.push_str(&render_component_body_str(component, args, base_indent));
    out
}

fn render_component_body_str(
    component: &ConnectedComponent,
    args: &GraphArgs,
    base_indent: &str,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for node in &component.nodes {
        let depth_indent = "  ".repeat(node.depth);
        let priority_badge = format!("[P{}]", node.priority);
        let status_badge = format!("[{}]", node.status);
        let root_marker = if node.depth == 0 { " (root)" } else { "" };
        let row_prefix = format!(
            "{base_indent}{depth_indent}{id}  {pri}  {stat}{root} ",
            id = node.id,
            pri = priority_badge,
            stat = status_badge,
            root = root_marker,
        );
        let used = row_prefix.chars().count();
        let sender_suffix = if !args.no_sender {
            node.sender
                .as_ref()
                .map(|s| format!(" (from: {s})"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let sender_w = sender_suffix.chars().count();
        let title_cap = title_budget(args, used + sender_w);
        let title = truncate_with_ellipsis(&node.title, title_cap);
        let _ = writeln!(out, "{row_prefix}{title}{sender_suffix}");
    }
    out
}

// Calculate depths for nodes using longest path from roots.
///
/// Roots are issues with no dependencies within the component.
/// Depth is the longest path from any root to the node.
fn calculate_depths(
    all_dependencies: &HashMap<String, Vec<crate::model::Dependency>>,
    nodes: &[String],
    component_set: &HashSet<&String>,
) -> HashMap<String, usize> {
    let mut depths: HashMap<String, usize> = HashMap::new();

    // Get dependencies for each node (filtered to component)
    let mut deps_map: HashMap<String, Vec<String>> = HashMap::new();
    // Also build reverse map (dependents) for efficient traversal
    let mut dependents_map: HashMap<String, Vec<String>> = HashMap::new();

    for node_id in nodes {
        if let Some(deps) = all_dependencies.get(node_id) {
            let filtered: Vec<String> = deps
                .iter()
                .filter(|d| d.dep_type.affects_ready_work())
                .map(|d| d.depends_on_id.clone())
                .filter(|d| component_set.contains(d))
                .collect();

            for dep_id in &filtered {
                dependents_map
                    .entry(dep_id.clone())
                    .or_default()
                    .push(node_id.clone());
            }

            deps_map.insert(node_id.clone(), filtered);
        } else {
            deps_map.insert(node_id.clone(), Vec::new());
        }
    }

    // Find roots (nodes with no dependencies in component)
    let roots: Vec<&String> = nodes
        .iter()
        .filter(|n| deps_map.get(*n).is_none_or(Vec::is_empty))
        .collect();

    // Max depth to prevent infinite loops in cycles
    let max_depth = nodes.len();

    // BFS from each root, tracking maximum depth
    for root in &roots {
        let mut queue: VecDeque<(&String, usize)> = VecDeque::new();
        queue.push_back((root, 0));

        while let Some((current, depth)) = queue.pop_front() {
            // Prevent infinite loops in cycles
            if depth > max_depth {
                continue;
            }

            // Update depth if this path is longer
            let entry = depths.entry(current.clone()).or_insert(0);
            if depth > *entry {
                *entry = depth;
            }

            // Find dependents (nodes that depend on current) using the reverse map
            if let Some(dependents) = dependents_map.get(current) {
                for dependent_id in dependents {
                    queue.push_back((dependent_id, depth + 1));
                }
            }
        }
    }

    // Ensure all nodes have a depth (isolated nodes get 0)
    for node_id in nodes {
        depths.entry(node_id.clone()).or_insert(0);
    }

    depths
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

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render single graph with rich formatting.
fn render_single_graph_rich(nodes: &[GraphNode], root_issue: &Issue, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Header with root info
    content.append_styled("Root: ", theme.dimmed.clone());
    content.append_styled(&root_issue.id, theme.issue_id.clone());
    content.append(" ");
    content.append_styled(&root_issue.title, theme.emphasis.clone());
    content.append("\n\n");

    // Dependent count
    let dep_count = nodes.len() - 1;
    content.append_styled(
        &format!(
            "{} dependent{}\n\n",
            dep_count,
            if dep_count == 1 { "" } else { "s" }
        ),
        theme.dimmed.clone(),
    );

    // Render tree
    for node in nodes {
        let indent = "  ".repeat(node.depth);

        // Depth indicator
        if node.depth == 0 {
            content.append_styled("● ", theme.success.clone());
        } else {
            content.append(&indent);
            content.append_styled("← ", theme.dimmed.clone());
        }

        // ID
        content.append_styled(&node.id, theme.issue_id.clone());
        content.append(" ");

        // Title
        content.append(&node.title);
        content.append(" ");

        // Priority badge
        let priority_style = priority_style(node.priority, theme);
        content.append_styled(&format!("[P{}]", node.priority), priority_style);
        content.append(" ");

        // Status badge
        let status_style = status_style(&node.status, theme);
        content.append_styled(&format!("[{}]", node.status), status_style);

        if node.depth == 0 {
            content.append_styled(" (root)", theme.dimmed.clone());
        }
        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render no dependents message with rich formatting.
fn render_no_dependents_rich(root_id: &str, root_issue: &Issue, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled("● ", theme.success.clone());
    content.append_styled(root_id, theme.issue_id.clone());
    content.append(" ");
    content.append(&root_issue.title);
    content.append("\n\n");
    content.append_styled("No dependents found", theme.dimmed.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

fn render_all_graph_rich_with_args(
    components: &[ConnectedComponent],
    total_nodes: usize,
    args: &GraphArgs,
    ctx: &OutputContext,
) {
    if args.no_cluster {
        render_all_graph_rich(components, total_nodes, args, ctx);
        return;
    }

    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled(
        &format!(
            "{total_nodes} issue{ip} across {comp_n} component{cp}\n",
            ip = if total_nodes == 1 { "" } else { "s" },
            comp_n = components.len(),
            cp = if components.len() == 1 { "" } else { "s" },
        ),
        theme.section.clone(),
    );

    let mut groups: BTreeMap<String, Vec<&ConnectedComponent>> = BTreeMap::new();
    for component in components {
        groups
            .entry(component_prefix(component))
            .or_default()
            .push(component);
    }
    let mut prefixes: Vec<String> = groups
        .keys()
        .filter(|p| *p != "<cross-prefix>")
        .cloned()
        .collect();
    prefixes.sort();
    if groups.contains_key("<cross-prefix>") {
        prefixes.push("<cross-prefix>".to_string());
    }

    let title_cap = args.max_title.unwrap_or(60);

    for prefix in &prefixes {
        let comps = groups.get(prefix).expect("present");
        let total_in_prefix: usize = comps.iter().map(|c| c.nodes.len()).sum();
        content.append("\n");
        content.append_styled(
            &format!(
                "{prefix} ({n} issue{ip}, {c} component{cp})\n",
                n = total_in_prefix,
                ip = if total_in_prefix == 1 { "" } else { "s" },
                c = comps.len(),
                cp = if comps.len() == 1 { "" } else { "s" },
            ),
            theme.emphasis.clone(),
        );

        for component in comps {
            for node in &component.nodes {
                let indent = "  ".repeat(node.depth + 1);
                content.append(&indent);
                content.append_styled(&node.id, theme.issue_id.clone());
                content.append(" ");
                let mut title = node.title.clone();
                if !args.no_sender {
                    if let Some(sender) = &node.sender {
                        title.push_str(&format!(" (from: {sender})"));
                    }
                }
                content.append(&truncate_with_ellipsis(&title, title_cap));
                content.append(" ");
                content.append_styled(
                    &format!("[P{}]", node.priority),
                    priority_style(node.priority, theme),
                );
                content.append(" ");
                content.append_styled(
                    &format!("[{}]", node.status),
                    status_style(&node.status, theme),
                );
                if node.depth == 0 {
                    content.append_styled(" (root)", theme.dimmed.clone());
                }
                content.append("\n");
            }
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Legacy flat rich rendering (used when --no-cluster). Title width now honors `--max-title`.
fn render_all_graph_rich(
    components: &[ConnectedComponent],
    total_nodes: usize,
    args: &GraphArgs,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Summary header
    content.append_styled(
        &format!(
            "{} issue{} in {} component{}\n",
            total_nodes,
            if total_nodes == 1 { "" } else { "s" },
            components.len(),
            if components.len() == 1 { "" } else { "s" }
        ),
        theme.section.clone(),
    );

    // Render each component
    for (i, component) in components.iter().enumerate() {
        content.append("\n");

        // Component header
        content.append_styled(&format!("Component {}", i + 1), theme.emphasis.clone());
        content.append_styled(
            &format!(
                " ({} issue{}, roots: {})\n",
                component.nodes.len(),
                if component.nodes.len() == 1 { "" } else { "s" },
                component.roots.join(", ")
            ),
            theme.dimmed.clone(),
        );

        // Render nodes in component
        let title_cap = args.max_title.unwrap_or(60);
        for node in &component.nodes {
            let indent = "  ".repeat(node.depth + 1);
            content.append(&indent);

            // ID
            content.append_styled(&node.id, theme.issue_id.clone());
            content.append(" ");

            // Title (with optional sender annotation, ellipsized to fit).
            let mut title = node.title.clone();
            if !args.no_sender {
                if let Some(sender) = &node.sender {
                    title.push_str(&format!(" (from: {sender})"));
                }
            }
            let title = truncate_with_ellipsis(&title, title_cap);
            content.append(&title);
            content.append(" ");

            // Priority badge
            let priority_style = priority_style(node.priority, theme);
            content.append_styled(&format!("[P{}]", node.priority), priority_style);
            content.append(" ");

            // Status badge
            let status_style = status_style(&node.status, theme);
            content.append_styled(&format!("[{}]", node.status), status_style);

            if node.depth == 0 {
                content.append_styled(" (root)", theme.dimmed.clone());
            }
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render no issues message with rich formatting.
fn render_no_issues_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled(
        "No open/in_progress/blocked/deferred issues found",
        theme.dimmed.clone(),
    );
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Get style for priority level.
fn priority_style(priority: i32, theme: &crate::output::Theme) -> Style {
    match priority {
        0 => theme.priority_critical.clone(),
        1 => theme.priority_high.clone(),
        2 => theme.priority_medium.clone(),
        3 => theme.priority_low.clone(),
        _ => theme.priority_backlog.clone(),
    }
}

/// Get style for status.
fn status_style(status: &str, theme: &crate::output::Theme) -> Style {
    match status {
        "open" => theme.status_open.clone(),
        "in_progress" => theme.status_in_progress.clone(),
        "blocked" => theme.status_blocked.clone(),
        "closed" => theme.status_closed.clone(),
        "deferred" => theme.status_deferred.clone(),
        _ => theme.dimmed.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_node_serialization() {
        let node = GraphNode {
            id: "bd-001".to_string(),
            title: "Test Issue".to_string(),
            status: "open".to_string(),
            priority: 2,
            depth: 0,
            sender: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"id\":\"bd-001\""));
        assert!(json.contains("\"depth\":0"));
    }

    #[test]
    fn test_single_graph_output_serialization() {
        let output = SingleGraphOutput {
            root: "bd-001".to_string(),
            count: 3,
            nodes: vec![
                GraphNode {
                    id: "bd-001".to_string(),
                    title: "Root".to_string(),
                    status: "open".to_string(),
                    priority: 2,
                    depth: 0,
            sender: None,
                },
                GraphNode {
                    id: "bd-002".to_string(),
                    title: "Child 1".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
            sender: None,
                },
            ],
            edges: vec![("bd-002".to_string(), "bd-001".to_string())],
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"root\":\"bd-001\""));
        assert!(json.contains("\"count\":3"));
    }

    #[test]
    fn test_connected_component_serialization() {
        let component = ConnectedComponent {
            nodes: vec![GraphNode {
                id: "bd-001".to_string(),
                title: "Test".to_string(),
                status: "open".to_string(),
                priority: 2,
                depth: 0,
            sender: None,
            }],
            edges: vec![],
            roots: vec!["bd-001".to_string()],
        };

        let json = serde_json::to_string(&component).unwrap();
        assert!(json.contains("\"roots\":[\"bd-001\"]"));
    }

    // ============================================================
    // Additional tests for comprehensive graph module coverage
    // ============================================================

    #[test]
    fn test_all_graph_output_serialization() {
        let output = AllGraphOutput {
            components: vec![ConnectedComponent {
                nodes: vec![
                    GraphNode {
                        id: "bd-001".to_string(),
                        title: "Root Issue".to_string(),
                        status: "open".to_string(),
                        priority: 1,
                        depth: 0,
            sender: None,
                    },
                    GraphNode {
                        id: "bd-002".to_string(),
                        title: "Child Issue".to_string(),
                        status: "blocked".to_string(),
                        priority: 2,
                        depth: 1,
            sender: None,
                    },
                ],
                edges: vec![("bd-002".to_string(), "bd-001".to_string())],
                roots: vec!["bd-001".to_string()],
            }],
            total_nodes: 2,
            total_components: 1,
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"total_nodes\":2"));
        assert!(json.contains("\"total_components\":1"));
        assert!(json.contains("\"components\""));
    }

    #[test]
    fn test_all_graph_output_empty() {
        let output = AllGraphOutput {
            components: vec![],
            total_nodes: 0,
            total_components: 0,
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"total_nodes\":0"));
        assert!(json.contains("\"total_components\":0"));
        assert!(json.contains("\"components\":[]"));
    }

    #[test]
    fn test_graph_node_all_fields_present() {
        let node = GraphNode {
            id: "beads_rust-abc123".to_string(),
            title: "Complex title with special chars: <>&".to_string(),
            status: "in_progress".to_string(),
            priority: 0,
            depth: 5,
            sender: None,
        };

        let json = serde_json::to_string(&node).unwrap();

        // Parse back to verify all fields
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["id"], "beads_rust-abc123");
        assert_eq!(parsed["title"], "Complex title with special chars: <>&");
        assert_eq!(parsed["status"], "in_progress");
        assert_eq!(parsed["priority"], 0);
        assert_eq!(parsed["depth"], 5);
    }

    #[test]
    fn test_graph_node_deserialize() {
        let json = r#"{
            "id": "bd-test",
            "title": "Test Issue",
            "status": "open",
            "priority": 2,
            "depth": 0
        }"#;

        // GraphNode doesn't derive Deserialize, but we can verify the JSON is valid
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["id"], "bd-test");
        assert_eq!(parsed["priority"], 2);
    }

    #[test]
    fn test_connected_component_with_multiple_roots() {
        let component = ConnectedComponent {
            nodes: vec![
                GraphNode {
                    id: "bd-001".to_string(),
                    title: "Root 1".to_string(),
                    status: "open".to_string(),
                    priority: 1,
                    depth: 0,
            sender: None,
                },
                GraphNode {
                    id: "bd-002".to_string(),
                    title: "Root 2".to_string(),
                    status: "open".to_string(),
                    priority: 2,
                    depth: 0,
            sender: None,
                },
                GraphNode {
                    id: "bd-003".to_string(),
                    title: "Shared Child".to_string(),
                    status: "blocked".to_string(),
                    priority: 3,
                    depth: 1,
            sender: None,
                },
            ],
            edges: vec![
                ("bd-003".to_string(), "bd-001".to_string()),
                ("bd-003".to_string(), "bd-002".to_string()),
            ],
            roots: vec!["bd-001".to_string(), "bd-002".to_string()],
        };

        let json = serde_json::to_string(&component).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Check roots array has both
        let roots = parsed["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);

        // Check edges array has both edges
        let edges = parsed["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn test_connected_component_empty() {
        let component = ConnectedComponent {
            nodes: vec![],
            edges: vec![],
            roots: vec![],
        };

        let json = serde_json::to_string(&component).unwrap();
        assert!(json.contains("\"nodes\":[]"));
        assert!(json.contains("\"edges\":[]"));
        assert!(json.contains("\"roots\":[]"));
    }

    #[test]
    fn test_single_graph_output_with_complex_edges() {
        let output = SingleGraphOutput {
            root: "bd-root".to_string(),
            count: 4,
            nodes: vec![
                GraphNode {
                    id: "bd-root".to_string(),
                    title: "Root".to_string(),
                    status: "open".to_string(),
                    priority: 0,
                    depth: 0,
            sender: None,
                },
                GraphNode {
                    id: "bd-a".to_string(),
                    title: "A".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
            sender: None,
                },
                GraphNode {
                    id: "bd-b".to_string(),
                    title: "B".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
            sender: None,
                },
                GraphNode {
                    id: "bd-c".to_string(),
                    title: "C".to_string(),
                    status: "blocked".to_string(),
                    priority: 2,
                    depth: 2,
            sender: None,
                },
            ],
            edges: vec![
                ("bd-a".to_string(), "bd-root".to_string()),
                ("bd-b".to_string(), "bd-root".to_string()),
                ("bd-c".to_string(), "bd-a".to_string()),
                ("bd-c".to_string(), "bd-b".to_string()),
            ],
        };

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["count"], 4);
        assert_eq!(parsed["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(parsed["edges"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn test_graph_node_priority_boundaries() {
        // Test P0 (critical)
        let p0_node = GraphNode {
            id: "bd-p0".to_string(),
            title: "Critical".to_string(),
            status: "open".to_string(),
            priority: 0,
            depth: 0,
            sender: None,
        };
        let json = serde_json::to_string(&p0_node).unwrap();
        assert!(json.contains("\"priority\":0"));

        // Test P4 (backlog)
        let p4_node = GraphNode {
            id: "bd-p4".to_string(),
            title: "Backlog".to_string(),
            status: "open".to_string(),
            priority: 4,
            depth: 0,
            sender: None,
        };
        let json = serde_json::to_string(&p4_node).unwrap();
        assert!(json.contains("\"priority\":4"));
    }

    #[test]
    fn test_all_graph_output_multiple_components() {
        let output = AllGraphOutput {
            components: vec![
                ConnectedComponent {
                    nodes: vec![GraphNode {
                        id: "comp1-a".to_string(),
                        title: "Comp1 Issue".to_string(),
                        status: "open".to_string(),
                        priority: 1,
                        depth: 0,
            sender: None,
                    }],
                    edges: vec![],
                    roots: vec!["comp1-a".to_string()],
                },
                ConnectedComponent {
                    nodes: vec![
                        GraphNode {
                            id: "comp2-a".to_string(),
                            title: "Comp2 Root".to_string(),
                            status: "open".to_string(),
                            priority: 2,
                            depth: 0,
            sender: None,
                        },
                        GraphNode {
                            id: "comp2-b".to_string(),
                            title: "Comp2 Child".to_string(),
                            status: "blocked".to_string(),
                            priority: 2,
                            depth: 1,
            sender: None,
                        },
                    ],
                    edges: vec![("comp2-b".to_string(), "comp2-a".to_string())],
                    roots: vec!["comp2-a".to_string()],
                },
            ],
            total_nodes: 3,
            total_components: 2,
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["total_components"], 2);
        assert_eq!(parsed["total_nodes"], 3);
        assert_eq!(parsed["components"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_graph_node_all_status_values() {
        let statuses = [
            "open",
            "in_progress",
            "blocked",
            "closed",
            "deferred",
            "tombstone",
        ];

        for status in statuses {
            let node = GraphNode {
                id: format!("bd-{status}"),
                title: format!("Issue with {status} status"),
                status: status.to_string(),
                priority: 2,
                depth: 0,
            sender: None,
            };

            let json = serde_json::to_string(&node).unwrap();
            assert!(json.contains(&format!("\"status\":\"{status}\"")));
        }
    }

    #[test]
    fn test_graph_all_cycle_robustness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        let root = Issue {
            id: "root".to_string(),
            title: "Root".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };
        let i1 = Issue {
            id: "bd-1".to_string(),
            title: "A".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };
        let i2 = Issue {
            id: "bd-2".to_string(),
            title: "B".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };

        storage.create_issue(&root, "test").unwrap();
        storage.create_issue(&i1, "test").unwrap();
        storage.create_issue(&i2, "test").unwrap();

        // Root blocks A
        storage
            .add_dependency("bd-1", "root", "waits-for", "test")
            .unwrap();
        // A blocks B
        storage
            .add_dependency("bd-2", "bd-1", "waits-for", "test")
            .unwrap();
        // B blocks A (cycle)
        storage
            .add_dependency("bd-1", "bd-2", "waits-for", "test")
            .unwrap();

        let ctx = OutputContext::from_flags(true, false, true); // JSON mode

        // This should not hang even with root feeding into cycle
        // If it hangs, the test runner will timeout
        let args = GraphArgs {
            all: true,
            ..Default::default()
        };
        let result = graph_all(&storage, &args, &ctx);
        assert!(result.is_ok());
    }
}
