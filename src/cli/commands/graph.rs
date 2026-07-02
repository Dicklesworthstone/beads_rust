//! Graph command implementation.
//!
//! Visualizes dependency graphs with focus on reverse dependencies (dependents).
//!
//! - `br graph <issue-id>`: Show all dependents of an issue (what depends on it)
//! - `br graph --all`: Show connected components for all nonterminal issues

use super::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    cli_for_routed_workspace, resolve_issue_id,
};
use crate::cli::GraphArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{IssueWithDependencyMetadata, format_status_label, sanitize_terminal_inline};
use crate::model::{Issue, Priority, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use tracing::debug;
use unicode_width::UnicodeWidthStr;

const GRAPH_DEPENDENTS_BATCH_SIZE: usize = 400;

/// JSON output for a single node in the graph.
#[derive(Debug, Clone, Serialize)]
struct GraphNode {
    id: String,
    title: String,
    status: String,
    priority: i32,
    depth: usize,
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

#[derive(Debug)]
struct SingleGraphTraversal {
    traversal_order: Vec<String>,
    issues_by_id: HashMap<String, Issue>,
    edges: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HumanGraphRenderMode {
    Rich,
    Compact,
    Plain,
}

/// Execute the graph command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(args: &GraphArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let route_cli = routed_cli_for_graph(cli, args, &beads_dir)?;
    let (storage_ctx, _routed_write_lock) = open_storage_for_graph(args, &route_cli, &beads_dir)?;
    execute_graph_with_storage_ctx(args, &route_cli, ctx, &storage_ctx)
}

fn routed_cli_for_graph(
    cli: &config::CliOverrides,
    args: &GraphArgs,
    local_beads_dir: &std::path::Path,
) -> Result<config::CliOverrides> {
    let is_external = if let Some(issue_input) = args.issue.as_deref()
        && !args.all
    {
        config::routing::resolve_route(issue_input, local_beads_dir)?.is_external
    } else {
        false
    };
    Ok(cli_for_routed_workspace(cli, is_external))
}

fn open_storage_for_graph(
    args: &GraphArgs,
    cli: &config::CliOverrides,
    local_beads_dir: &std::path::Path,
) -> Result<(config::OpenStorageResult, super::RoutedWorkspaceWriteLock)> {
    if let Some(issue_input) = args.issue.as_deref()
        && !args.all
    {
        let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
        let mut route_cli = cli_for_routed_workspace(cli, route.is_external);
        let routed_write_lock = acquire_routed_workspace_write_lock(
            &route.beads_dir,
            route.is_external,
            route_cli.lock_timeout,
        )?;
        routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
        let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
        auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli)?;
        return Ok((storage_ctx, routed_write_lock));
    }

    let routed_write_lock = super::RoutedWorkspaceWriteLock::local();
    let mut storage_ctx = config::open_storage_with_cli(local_beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;
    Ok((storage_ctx, routed_write_lock))
}

/// Execute the graph command using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute_with_storage_ctx(
    args: &GraphArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &std::path::Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    if let Some(issue_input) = args.issue.as_deref()
        && !args.all
    {
        let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
        if route.is_external {
            let mut route_cli = cli_for_routed_workspace(cli, true);
            let routed_write_lock = acquire_routed_workspace_write_lock(
                &route.beads_dir,
                true,
                route_cli.lock_timeout,
            )?;
            routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
            let mut routed_storage_ctx =
                config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
            auto_import_storage_ctx_if_stale(&mut routed_storage_ctx, &route_cli)?;
            return execute_graph_with_storage_ctx(args, &route_cli, ctx, &routed_storage_ctx);
        }
    }

    execute_graph_with_storage_ctx(args, cli, ctx, storage_ctx)
}

fn execute_graph_with_storage_ctx(
    args: &GraphArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    if args.all {
        graph_all(&storage_ctx.storage, args.compact, args.dot, ctx)
    } else {
        let issue_id = args.issue.as_ref().ok_or_else(|| {
            BeadsError::validation("issue", "Issue ID required unless --all is specified")
        })?;

        let resolved_id = resolve_issue_id(&storage_ctx.storage, &resolver, issue_id)?;
        graph_single(
            &storage_ctx.storage,
            &resolved_id,
            args.compact,
            args.dot,
            ctx,
        )
    }
}

/// Show graph for a single issue (traverse dependents only).
fn graph_single(
    storage: &SqliteStorage,
    root_id: &str,
    compact: bool,
    dot: bool,
    ctx: &OutputContext,
) -> Result<()> {
    // Verify the root issue exists
    let root_issue = storage
        .get_issue(root_id)?
        .ok_or_else(|| BeadsError::IssueNotFound {
            id: root_id.to_string(),
        })?;

    if ctx.is_quiet() {
        return Ok(());
    }

    let traversal = collect_single_graph(storage, root_id, &root_issue)?;
    let root_nodes = [root_id.to_string()];
    let depths = calculate_depths_from_dependency_edges(
        &traversal.traversal_order,
        &traversal.edges,
        &root_nodes,
    );
    let mut nodes = build_graph_nodes(&traversal.traversal_order, &traversal.issues_by_id, &depths);
    sort_single_graph_nodes(&mut nodes, root_id);

    // DOT/Graphviz output takes precedence over text/JSON rendering.
    if dot {
        print!(
            "{}",
            format_single_graph_dot(&nodes, &traversal.edges, root_id)
        );
        return Ok(());
    }

    if ctx.is_json() || ctx.is_toon() {
        let output = SingleGraphOutput {
            root: root_id.to_string(),
            count: nodes.len(),
            nodes,
            edges: traversal.edges,
        };
        if ctx.is_toon() {
            ctx.toon(&output);
        } else {
            ctx.json_pretty(&output);
        }
        return Ok(());
    }

    // Text output
    if nodes.len() == 1 {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_dependents_rich(root_id, &root_issue, ctx);
        } else {
            println!("No dependents for {}", graph_display_text(root_id));
        }
        return Ok(());
    }

    match human_graph_render_mode(ctx.mode(), compact) {
        HumanGraphRenderMode::Rich => {
            render_single_graph_rich(&nodes, &traversal.edges, &root_issue, ctx);
        }
        HumanGraphRenderMode::Compact => {
            println!(
                "{}",
                format_compact_dependency_edges(root_id, &traversal.edges)
            );
        }
        HumanGraphRenderMode::Plain => {
            render_single_graph_plain(&nodes, &traversal.edges, &root_issue);
        }
    }

    Ok(())
}

/// Show graph for all nonterminal issues.
#[allow(clippy::too_many_lines)]
fn graph_all(storage: &SqliteStorage, compact: bool, dot: bool, ctx: &OutputContext) -> Result<()> {
    if ctx.is_quiet() {
        return Ok(());
    }

    // Get all nonterminal issues
    let filters = ListFilters {
        include_closed: false,
        include_deferred: true,
        include_templates: false,
        ..Default::default()
    };

    let issues = storage.list_graph_issues_for_command_output(&filters)?;
    debug!(count = issues.len(), "Found issues for graph");

    if issues.is_empty() {
        if dot {
            print!("{}", format_all_graph_dot(&[]));
        } else if ctx.is_json() || ctx.is_toon() {
            let output = AllGraphOutput {
                components: vec![],
                total_nodes: 0,
                total_components: 0,
            };
            if ctx.is_toon() {
                ctx.toon(&output);
            } else {
                ctx.json_pretty(&output);
            }
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_no_issues_rich(ctx);
        } else {
            println!("No active issues found");
        }
        return Ok(());
    }

    // Build issue lookup and adjacency lists
    let issue_set: HashSet<String> = issues.iter().map(|i| i.id.clone()).collect();
    let issue_map: HashMap<String, &crate::model::Issue> =
        issues.iter().map(|i| (i.id.clone(), i)).collect();

    // Build adjacency list (both directions for connected components)
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut blocking_edges: Vec<(String, String)> = Vec::new();

    // Optimize: fetch all dependencies once
    let all_dependencies = storage.get_all_dependency_records()?;

    for issue in &issues {
        adj.entry(issue.id.clone()).or_default();

        // Get dependencies from bulk map
        if let Some(deps) = all_dependencies.get(&issue.id) {
            for dependency in deps {
                if !dependency.dep_type.affects_ready_work() {
                    continue;
                }
                let (dependent_id, dependency_id) = if matches!(
                    dependency.dep_type,
                    crate::model::DependencyType::ParentChild
                ) {
                    (dependency.depends_on_id.as_str(), issue.id.as_str())
                } else {
                    (issue.id.as_str(), dependency.depends_on_id.as_str())
                };
                // Only include edges within our issue set
                if issue_set.contains(dependency_id) && issue_set.contains(dependent_id) {
                    adj.entry(dependent_id.to_string())
                        .or_default()
                        .push(dependency_id.to_string());
                    adj.entry(dependency_id.to_string())
                        .or_default()
                        .push(dependent_id.to_string());
                    blocking_edges.push((dependent_id.to_string(), dependency_id.to_string()));
                }
            }
        }
    }

    // Find connected components using BFS
    let mut visited: HashSet<String> = HashSet::new();
    let mut components: Vec<ConnectedComponent> = Vec::new();

    for issue in &issues {
        if visited.contains(&issue.id) {
            continue;
        }

        // BFS to find all nodes in this component
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

        // Calculate depths using longest path from roots
        // Roots are issues with no unsatisfied dependencies within the component
        let component_set: HashSet<&str> = component_nodes.iter().map(String::as_str).collect();
        let (mut depths, mut roots) =
            calculate_depths(&all_dependencies, &component_nodes, &component_set);

        // Build component output
        let mut nodes: Vec<GraphNode> = Vec::new();

        for node_id in &component_nodes {
            if let Some(issue) = issue_map.get(node_id) {
                let depth = depths.remove(node_id).unwrap_or(0);
                nodes.push(GraphNode {
                    id: node_id.clone(),
                    title: issue.title.clone(),
                    status: issue.status.as_str().to_string(),
                    priority: issue.priority.0,
                    depth,
                });
            }
        }

        // Sort by depth, priority, id
        nodes.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then(a.priority.cmp(&b.priority))
                .then(a.id.cmp(&b.id))
        });
        roots.sort();

        // Filter edges to this component
        let component_edges: Vec<(String, String)> = blocking_edges
            .iter()
            .filter(|(from, to)| {
                component_set.contains(from.as_str()) && component_set.contains(to.as_str())
            })
            .cloned()
            .collect();

        components.push(ConnectedComponent {
            nodes,
            edges: component_edges,
            roots,
        });
    }

    // Sort components by size (largest first)
    components.sort_by_key(|b| std::cmp::Reverse(b.nodes.len()));

    let total_nodes: usize = components.iter().map(|c| c.nodes.len()).sum();

    // DOT/Graphviz output takes precedence over text/JSON rendering.
    if dot {
        print!("{}", format_all_graph_dot(&components));
        return Ok(());
    }

    if ctx.is_json() || ctx.is_toon() {
        let output = AllGraphOutput {
            total_nodes,
            total_components: components.len(),
            components,
        };
        if ctx.is_toon() {
            ctx.toon(&output);
        } else {
            ctx.json_pretty(&output);
        }
        return Ok(());
    }

    // Text output
    match human_graph_render_mode(ctx.mode(), compact) {
        HumanGraphRenderMode::Rich => render_all_graph_rich(&components, total_nodes, ctx),
        HumanGraphRenderMode::Compact | HumanGraphRenderMode::Plain => {
            let render_compact = matches!(
                human_graph_render_mode(ctx.mode(), compact),
                HumanGraphRenderMode::Compact
            );

            println!(
                "Dependency graph: {} issues in {} component(s)",
                total_nodes,
                components.len()
            );
            println!();

            for (i, component) in components.iter().enumerate() {
                if render_compact {
                    // Compact: one line per component
                    let ids = format_id_iter(component.nodes.iter().map(|node| node.id.as_str()));
                    println!("Component {}: {}", i + 1, ids);
                } else {
                    let roots = if component.roots.is_empty() {
                        "none".to_string()
                    } else {
                        format_id_list(&component.roots)
                    };
                    let parent_map = build_parent_map(&component.edges);
                    // Detailed view
                    println!(
                        "Component {} ({} issues, roots: {}, by depth):",
                        i + 1,
                        component.nodes.len(),
                        roots
                    );

                    for node in &component.nodes {
                        let indent = "  ".repeat(node.depth + 1);
                        let root_marker = if component.roots.contains(&node.id) {
                            " (root)"
                        } else {
                            ""
                        };
                        let parents =
                            format_parent_list(parent_map.get(&node.id).map(Vec::as_slice));
                        println!(
                            "{}{}: {} [P{}] [{}]{}{}",
                            indent,
                            graph_display_text(&node.id),
                            sanitize_terminal_inline(&node.title),
                            node.priority,
                            sanitize_terminal_inline(&node.status),
                            root_marker,
                            parents
                        );
                    }
                    println!();
                }
            }
        }
    }

    Ok(())
}

// Calculate depths for nodes using longest path from roots.
///
/// Returns both the computed depths and the component roots. Roots are issues
/// with no dependencies within the component. Depth is the longest path from
/// any root to the node.
fn calculate_depths(
    all_dependencies: &HashMap<String, Vec<crate::model::Dependency>>,
    nodes: &[String],
    component_set: &HashSet<&str>,
) -> (HashMap<String, usize>, Vec<String>) {
    let dependency_edges = dependency_edges_for_component(all_dependencies, nodes, component_set);
    let dependency_map = build_dependency_map(nodes, &dependency_edges);
    let roots: Vec<String> = nodes
        .iter()
        .filter(|node_id| dependency_map.get(*node_id).is_none_or(Vec::is_empty))
        .cloned()
        .collect();
    let depths = calculate_depths_from_dependency_edges(nodes, &dependency_edges, &roots);
    (depths, roots)
}

fn dependency_edges_for_component(
    all_dependencies: &HashMap<String, Vec<crate::model::Dependency>>,
    nodes: &[String],
    component_set: &HashSet<&str>,
) -> Vec<(String, String)> {
    let mut dependency_edges = Vec::new();

    for node_id in nodes {
        if let Some(deps) = all_dependencies.get(node_id) {
            for dependency in deps {
                if !dependency.dep_type.affects_ready_work() {
                    continue;
                }

                let (dependent_id, dependency_id) = if matches!(
                    dependency.dep_type,
                    crate::model::DependencyType::ParentChild
                ) {
                    (dependency.depends_on_id.as_str(), node_id.as_str())
                } else {
                    (node_id.as_str(), dependency.depends_on_id.as_str())
                };

                if component_set.contains(dependency_id) && component_set.contains(dependent_id) {
                    dependency_edges.push((dependent_id.to_string(), dependency_id.to_string()));
                }
            }
        }
    }

    dependency_edges
}

fn collect_single_graph(
    storage: &SqliteStorage,
    root_id: &str,
    root_issue: &Issue,
) -> Result<SingleGraphTraversal> {
    // DFS traversal producing first-visit order so rendered subtrees stay contiguous.
    // Cycle prevention uses expanded_nodes (fully processed) and queued_nodes (on stack)
    // — together they prevent any node from being pushed twice, making the per-entry
    // path Vec unnecessary.
    let mut traversal_order: Vec<String> = Vec::new();
    let mut issues_by_id: HashMap<String, Issue> = HashMap::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut seen_edges: HashSet<(String, String)> = HashSet::new();
    let mut ordered_nodes: HashSet<String> = HashSet::new();
    let mut expanded_nodes: HashSet<String> = HashSet::new();
    let mut queued_nodes: HashSet<String> = HashSet::new();

    let metadata_cache = storage.get_active_issues_metadata()?;
    let mut dependents_cache: HashMap<String, Vec<IssueWithDependencyMetadata>> = HashMap::new();

    let mut stack: Vec<String> = vec![root_id.to_string()];
    queued_nodes.insert(root_id.to_string());

    issues_by_id.insert(root_id.to_string(), root_issue.clone());

    while let Some(current_id) = stack.pop() {
        queued_nodes.remove(&current_id);

        if ordered_nodes.insert(current_id.clone()) {
            traversal_order.push(current_id.clone());
        }

        if !expanded_nodes.insert(current_id.clone()) {
            continue;
        }

        let mut frontier_batch = Vec::with_capacity(stack.len().saturating_add(1));
        frontier_batch.push(current_id.clone());
        frontier_batch.extend(stack.iter().rev().cloned());
        cache_graph_dependents(storage, &mut dependents_cache, &frontier_batch)?;

        let mut dependents: Vec<_> = dependents_cache
            .get(&current_id)
            .cloned()
            .unwrap_or_default();
        dependents.sort_by(|a, b| a.priority.0.cmp(&b.priority.0).then(a.id.cmp(&b.id)));

        for dep in dependents.into_iter().rev() {
            let edge = (dep.id.clone(), current_id.clone());
            if seen_edges.insert(edge.clone()) {
                edges.push(edge);
            }

            if !issues_by_id.contains_key(&dep.id) {
                let issue = if let Some(meta) = metadata_cache.get(&dep.id) {
                    Issue {
                        id: dep.id.clone(),
                        title: meta.0.clone(),
                        priority: Priority(meta.1),
                        status: Status::from_str(&meta.2).unwrap_or(Status::Open),
                        ..Issue::default()
                    }
                } else {
                    Issue {
                        id: dep.id.clone(),
                        title: dep.title.clone(),
                        priority: dep.priority,
                        status: dep.status.clone(),
                        ..Issue::default()
                    }
                };

                issues_by_id.insert(dep.id.clone(), issue);
            }

            if dep.id != current_id
                && !expanded_nodes.contains(&dep.id)
                && queued_nodes.insert(dep.id.clone())
            {
                stack.push(dep.id.clone());
            }
        }
    }

    Ok(SingleGraphTraversal {
        traversal_order,
        issues_by_id,
        edges,
    })
}

fn cache_graph_dependents(
    storage: &SqliteStorage,
    dependents_cache: &mut HashMap<String, Vec<IssueWithDependencyMetadata>>,
    issue_ids: &[String],
) -> Result<()> {
    let mut missing_ids = Vec::new();
    let mut seen_ids = HashSet::new();
    for issue_id in issue_ids {
        if dependents_cache.contains_key(issue_id.as_str()) || !seen_ids.insert(issue_id.as_str()) {
            continue;
        }
        missing_ids.push(issue_id.clone());
        if missing_ids.len() == GRAPH_DEPENDENTS_BATCH_SIZE {
            break;
        }
    }

    if missing_ids.is_empty() {
        return Ok(());
    }

    let fetched = storage.get_blocking_dependents_for_issue_ids(&missing_ids)?;
    for issue_id in &missing_ids {
        dependents_cache.insert(
            issue_id.clone(),
            fetched.get(issue_id).cloned().unwrap_or_default(),
        );
    }

    Ok(())
}

fn build_graph_nodes(
    traversal_order: &[String],
    issues_by_id: &HashMap<String, Issue>,
    depths: &HashMap<String, usize>,
) -> Vec<GraphNode> {
    traversal_order
        .iter()
        .map(|node_id| {
            let issue = issues_by_id
                .get(node_id)
                .expect("reachable graph node should exist");
            GraphNode {
                id: node_id.clone(),
                title: issue.title.clone(),
                status: issue.status.as_str().to_string(),
                priority: issue.priority.0,
                depth: depths.get(node_id).copied().unwrap_or(0),
            }
        })
        .collect()
}

fn sort_single_graph_nodes(nodes: &mut [GraphNode], root_id: &str) {
    if let Some(root_index) = nodes.iter().position(|node| node.id == root_id) {
        let prefix_len = root_index.saturating_add(1);
        if let Some(prefix) = nodes.get_mut(..prefix_len) {
            prefix.rotate_right(1);
        }
    }
}

fn calculate_depths_from_dependency_edges(
    nodes: &[String],
    dependency_edges: &[(String, String)],
    root_nodes: &[String],
) -> HashMap<String, usize> {
    let dependency_map = build_dependency_map(nodes, dependency_edges);
    let node_to_component = strongly_connected_components(nodes, &dependency_map);
    let component_count = node_to_component
        .values()
        .copied()
        .max()
        .map_or(0, |max_component| max_component + 1);

    if component_count == 0 {
        return HashMap::new();
    }

    let mut component_children: HashMap<usize, HashSet<usize>> = HashMap::new();
    let mut component_indegree = vec![0usize; component_count];

    for (dependent, dependency) in dependency_edges {
        let Some(&dependent_component) = node_to_component.get(dependent) else {
            continue;
        };
        let Some(&dependency_component) = node_to_component.get(dependency) else {
            continue;
        };

        if dependency_component == dependent_component {
            continue;
        }

        let inserted = component_children
            .entry(dependency_component)
            .or_default()
            .insert(dependent_component);
        if inserted && let Some(indegree) = component_indegree.get_mut(dependent_component) {
            *indegree += 1;
        }
    }

    let mut component_depths = vec![0usize; component_count];
    let mut reachable_components: HashSet<usize> = root_nodes
        .iter()
        .filter_map(|node_id| node_to_component.get(node_id).copied())
        .collect();

    let mut queue: VecDeque<usize> = component_indegree
        .iter()
        .enumerate()
        .filter_map(|(component_id, indegree)| (*indegree == 0).then_some(component_id))
        .collect();

    while let Some(component_id) = queue.pop_front() {
        if let Some(children) = component_children.get(&component_id) {
            for child_component in children {
                if reachable_components.contains(&component_id) {
                    reachable_components.insert(*child_component);
                    let parent_depth = component_depths.get(component_id).copied().unwrap_or(0);
                    if let Some(child_depth) = component_depths.get_mut(*child_component) {
                        *child_depth = (*child_depth).max(parent_depth + 1);
                    }
                }

                if let Some(indegree) = component_indegree.get_mut(*child_component) {
                    *indegree = indegree.saturating_sub(1);
                    if *indegree == 0 {
                        queue.push_back(*child_component);
                    }
                }
            }
        }
    }

    nodes
        .iter()
        .map(|node_id| {
            let depth = node_to_component
                .get(node_id)
                .and_then(|component_id| component_depths.get(*component_id))
                .copied()
                .unwrap_or(0);
            (node_id.clone(), depth)
        })
        .collect()
}

fn build_dependency_map(
    nodes: &[String],
    dependency_edges: &[(String, String)],
) -> HashMap<String, Vec<String>> {
    let mut dependency_map: HashMap<String, Vec<String>> = nodes
        .iter()
        .cloned()
        .map(|node_id| (node_id, Vec::new()))
        .collect();

    for (dependent, dependency) in dependency_edges {
        dependency_map
            .entry(dependent.clone())
            .or_default()
            .push(dependency.clone());
    }

    for dependencies in dependency_map.values_mut() {
        dependencies.sort();
        dependencies.dedup();
    }

    dependency_map
}

fn strongly_connected_components(
    nodes: &[String],
    dependency_map: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    let reverse_map = build_reverse_map(nodes, dependency_map);
    let mut visited: HashSet<String> = HashSet::new();
    let mut finish_order = Vec::new();

    for node_id in nodes {
        if visited.contains(node_id) {
            continue;
        }
        dfs_finish_order(node_id, dependency_map, &mut visited, &mut finish_order);
    }

    let mut component_map = HashMap::new();
    let mut next_component = 0usize;

    while let Some(node_id) = finish_order.pop() {
        if component_map.contains_key(&node_id) {
            continue;
        }

        let mut stack = vec![node_id.clone()];
        while let Some(current_id) = stack.pop() {
            if component_map.contains_key(&current_id) {
                continue;
            }

            component_map.insert(current_id.clone(), next_component);
            if let Some(parents) = reverse_map.get(&current_id) {
                for parent_id in parents.iter().rev() {
                    if !component_map.contains_key(parent_id) {
                        stack.push(parent_id.clone());
                    }
                }
            }
        }

        next_component += 1;
    }

    component_map
}

fn build_reverse_map(
    nodes: &[String],
    dependency_map: &HashMap<String, Vec<String>>,
) -> HashMap<String, Vec<String>> {
    let mut reverse_map: HashMap<String, Vec<String>> = nodes
        .iter()
        .cloned()
        .map(|node_id| (node_id, Vec::new()))
        .collect();

    for (node_id, dependencies) in dependency_map {
        for dependency_id in dependencies {
            reverse_map
                .entry(dependency_id.clone())
                .or_default()
                .push(node_id.clone());
        }
    }

    for dependents in reverse_map.values_mut() {
        dependents.sort();
        dependents.dedup();
    }

    reverse_map
}

fn dfs_finish_order(
    start: &str,
    dependency_map: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    finish_order: &mut Vec<String>,
) {
    let mut stack = vec![(start.to_string(), false)];

    while let Some((node_id, expanded)) = stack.pop() {
        if expanded {
            finish_order.push(node_id);
            continue;
        }

        if !visited.insert(node_id.clone()) {
            continue;
        }

        stack.push((node_id.clone(), true));
        if let Some(dependencies) = dependency_map.get(&node_id) {
            for dependency_id in dependencies.iter().rev() {
                if !visited.contains(dependency_id) {
                    stack.push((dependency_id.clone(), false));
                }
            }
        }
    }
}

fn build_parent_map(edges: &[(String, String)]) -> HashMap<String, Vec<String>> {
    let mut parent_map: HashMap<String, Vec<String>> = HashMap::new();

    for (dependent, dependency) in edges {
        parent_map
            .entry(dependent.clone())
            .or_default()
            .push(dependency.clone());
    }

    for parents in parent_map.values_mut() {
        parents.sort();
        parents.dedup();
    }

    parent_map
}

fn graph_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn format_id_iter<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut rendered = String::new();
    for id in ids {
        if !rendered.is_empty() {
            rendered.push_str(", ");
        }
        rendered.push_str(&graph_display_text(id));
    }
    rendered
}

fn format_id_list(ids: &[String]) -> String {
    format_id_iter(ids.iter().map(String::as_str))
}

fn format_parent_list(parents: Option<&[String]>) -> String {
    match parents {
        Some(parents) if !parents.is_empty() => {
            format!(" depends on: {}", format_id_list(parents))
        }
        _ => String::new(),
    }
}

fn format_compact_dependency_edges(root_id: &str, edges: &[(String, String)]) -> String {
    if edges.is_empty() {
        return graph_display_text(root_id);
    }

    let mut grouped_dependents: HashMap<String, Vec<String>> = HashMap::new();
    for (dependent, dependency) in edges {
        grouped_dependents
            .entry(dependency.clone())
            .or_default()
            .push(dependent.clone());
    }

    let mut dependencies: Vec<String> = grouped_dependents.keys().cloned().collect();
    dependencies.sort_by(|left, right| {
        if left == root_id {
            std::cmp::Ordering::Less
        } else if right == root_id {
            std::cmp::Ordering::Greater
        } else {
            left.cmp(right)
        }
    });
    let includes_root = dependencies
        .iter()
        .any(|dependency_id| dependency_id == root_id);

    let parts: Vec<String> = dependencies
        .iter()
        .map(|dependency_id| {
            let dependents = grouped_dependents
                .get(dependency_id)
                .expect("grouped dependents should exist");
            let mut sorted_dependents = dependents.clone();
            sorted_dependents.sort();
            sorted_dependents.dedup();
            format!(
                "{} <- {}",
                graph_display_text(dependency_id),
                format_id_list(&sorted_dependents)
            )
        })
        .collect();

    if includes_root {
        parts.join("; ")
    } else {
        format!("{}; {}", graph_display_text(root_id), parts.join("; "))
    }
}

const fn human_graph_render_mode(mode: OutputMode, compact: bool) -> HumanGraphRenderMode {
    if compact {
        HumanGraphRenderMode::Compact
    } else if matches!(mode, OutputMode::Rich) {
        HumanGraphRenderMode::Rich
    } else {
        HumanGraphRenderMode::Plain
    }
}

fn render_single_graph_plain(nodes: &[GraphNode], edges: &[(String, String)], root_issue: &Issue) {
    let parent_map = build_parent_map(edges);
    println!(
        "Dependents of {} by depth ({} total):",
        graph_display_text(&root_issue.id),
        nodes.len() - 1
    );
    println!();
    println!(
        "  {}: {} [P{}] [{}] (root)",
        graph_display_text(&root_issue.id),
        sanitize_terminal_inline(&root_issue.title),
        root_issue.priority.0,
        format_status_label(&root_issue.status, false)
    );

    for node in nodes.iter().skip(1) {
        let indent = "  ".repeat(node.depth + 1);
        let parents = format_parent_list(parent_map.get(&node.id).map(Vec::as_slice));
        println!(
            "{}← {}: {} [P{}] [{}]{}",
            indent,
            graph_display_text(&node.id),
            sanitize_terminal_inline(&node.title),
            node.priority,
            sanitize_terminal_inline(&node.status),
            parents
        );
    }
}

// ─────────────────────────────────────────────────────────────
// Graphviz DOT Output Rendering
// ─────────────────────────────────────────────────────────────

/// Escape a string for use inside a double-quoted Graphviz DOT literal.
///
/// First strips terminal control sequences (sharing the same hardening as the
/// human renderers), then escapes the DOT metacharacters `\` and `"` so the
/// emitted document stays well-formed regardless of issue id/title contents.
fn sanitize_dot_string(text: &str) -> String {
    sanitize_terminal_inline(text)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Graphviz fill color for a status (named X11 colors understood by `dot`).
fn dot_status_fillcolor(status: &str) -> &'static str {
    match status {
        "open" => "lightblue",
        "in_progress" => "khaki",
        "blocked" => "lightcoral",
        "deferred" => "lightcyan",
        "closed" => "lightgrey",
        "tombstone" => "grey",
        _ => "white",
    }
}

/// Append one DOT node statement for `node` at the given indentation.
///
/// The root (or a component root) is drawn with a heavier border so it stands
/// out in the rendered image.
fn push_dot_node(out: &mut String, indent: &str, node: &GraphNode, is_root: bool) {
    let label = format!(
        "{}\\n{} [P{}]",
        sanitize_dot_string(&node.id),
        sanitize_dot_string(&node.title),
        node.priority
    );
    let root_attr = if is_root { ", penwidth=2" } else { "" };
    out.push_str(&format!(
        "{indent}\"{}\" [label=\"{}\", fillcolor=\"{}\"{}];\n",
        sanitize_dot_string(&node.id),
        label,
        dot_status_fillcolor(&node.status),
        root_attr
    ));
}

/// Append one DOT edge statement for a dependency relationship.
///
/// Edges are stored as `(dependent, dependency)`; the arrow points from the
/// dependent to what it depends on (matching the `br dep --format mermaid`
/// convention), so `A -> B` reads "A depends on B".
fn push_dot_edge(out: &mut String, dependent: &str, dependency: &str) {
    out.push_str(&format!(
        "    \"{}\" -> \"{}\";\n",
        sanitize_dot_string(dependent),
        sanitize_dot_string(dependency)
    ));
}

/// Format the single-issue dependents graph as a Graphviz DOT document.
fn format_single_graph_dot(
    nodes: &[GraphNode],
    edges: &[(String, String)],
    root_id: &str,
) -> String {
    let mut out = String::new();
    out.push_str("digraph dependencies {\n");
    out.push_str("    node [shape=box, style=\"rounded,filled\", fontname=\"sans-serif\"];\n");
    for node in nodes {
        push_dot_node(&mut out, "    ", node, node.id == root_id);
    }
    for (dependent, dependency) in edges {
        push_dot_edge(&mut out, dependent, dependency);
    }
    out.push_str("}\n");
    out
}

/// Format the all-issues connected-component graph as a Graphviz DOT document.
///
/// Each connected component becomes its own `subgraph cluster_N` so the
/// rendered image visually groups independent dependency islands.
fn format_all_graph_dot(components: &[ConnectedComponent]) -> String {
    let mut out = String::new();
    out.push_str("digraph dependencies {\n");
    out.push_str("    node [shape=box, style=\"rounded,filled\", fontname=\"sans-serif\"];\n");
    for (index, component) in components.iter().enumerate() {
        out.push_str(&format!("    subgraph cluster_{index} {{\n"));
        out.push_str(&format!("        label=\"Component {}\";\n", index + 1));
        for node in &component.nodes {
            push_dot_node(
                &mut out,
                "        ",
                node,
                component.roots.contains(&node.id),
            );
        }
        out.push_str("    }\n");
    }
    for component in components {
        for (dependent, dependency) in &component.edges {
            push_dot_edge(&mut out, dependent, dependency);
        }
    }
    out.push_str("}\n");
    out
}

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render single graph with rich formatting.
fn render_single_graph_rich(
    nodes: &[GraphNode],
    edges: &[(String, String)],
    root_issue: &Issue,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();
    let parent_map = build_parent_map(edges);

    let mut content = Text::new("");

    // Header with root info
    content.append_styled("Root: ", theme.dimmed.clone());
    content.append_styled(&graph_display_text(&root_issue.id), theme.issue_id.clone());
    content.append(" ");
    content.append_styled(
        sanitize_terminal_inline(&root_issue.title).as_ref(),
        theme.emphasis.clone(),
    );
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
    content.append_styled(
        "Layered view; shared dependents list all blockers in this graph.\n\n",
        theme.dimmed.clone(),
    );

    // Render tree
    for node in nodes {
        let indent = "  ".repeat(node.depth);

        // Depth indicator
        if node.id == root_issue.id {
            content.append_styled("● ", theme.success.clone());
        } else {
            content.append(&indent);
            content.append_styled("← ", theme.dimmed.clone());
        }

        // ID
        content.append_styled(&graph_display_text(&node.id), theme.issue_id.clone());
        content.append(" ");

        // Title
        content.append(sanitize_terminal_inline(&node.title).as_ref());
        content.append(" ");

        // Priority badge
        let priority_style = priority_style(node.priority, theme);
        content.append_styled(&format!("[P{}]", node.priority), priority_style);
        content.append(" ");

        // Status badge
        let status_style = status_style(&node.status, theme);
        content.append_styled(
            &format!("[{}]", sanitize_terminal_inline(&node.status)),
            status_style,
        );

        if node.id == root_issue.id {
            content.append_styled(" (root)", theme.dimmed.clone());
        } else if let Some(parents) = parent_map.get(&node.id) {
            content.append_styled(&format_parent_list(Some(parents)), theme.dimmed.clone());
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
    content.append_styled(&graph_display_text(root_id), theme.issue_id.clone());
    content.append(" ");
    content.append(sanitize_terminal_inline(&root_issue.title).as_ref());
    content.append("\n\n");
    content.append_styled("No dependents found", theme.dimmed.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dependency Graph", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render all graph (connected components) with rich formatting.
fn render_all_graph_rich(
    components: &[ConnectedComponent],
    total_nodes: usize,
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
    content.append_styled(
        "Layered view; shared dependents list all blockers within each component.\n",
        theme.dimmed.clone(),
    );

    // Render each component
    for (i, component) in components.iter().enumerate() {
        content.append("\n");
        let parent_map = build_parent_map(&component.edges);

        // Component header
        content.append_styled(&format!("Component {}", i + 1), theme.emphasis.clone());
        let roots = if component.roots.is_empty() {
            "none".to_string()
        } else {
            format_id_list(&component.roots)
        };
        content.append_styled(
            &format!(
                " ({} issue{}, roots: {})\n",
                component.nodes.len(),
                if component.nodes.len() == 1 { "" } else { "s" },
                roots
            ),
            theme.dimmed.clone(),
        );

        // Render nodes in component
        for node in &component.nodes {
            let indent = "  ".repeat(node.depth + 1);
            content.append(&indent);

            // ID
            content.append_styled(&graph_display_text(&node.id), theme.issue_id.clone());
            content.append(" ");

            // Title (truncate if too long for all-graph view to avoid messy panels)
            // Use visual width for the check to be consistent with truncate_title
            let title = if UnicodeWidthStr::width(node.title.as_str()) > 40 {
                crate::format::truncate_title(&node.title, 40)
            } else {
                sanitize_terminal_inline(&node.title).into_owned()
            };
            content.append(&title);
            content.append(" ");

            // Priority badge
            let priority_style = priority_style(node.priority, theme);
            content.append_styled(&format!("[P{}]", node.priority), priority_style);
            content.append(" ");

            // Status badge
            let status_style = status_style(&node.status, theme);
            content.append_styled(
                &format!("[{}]", sanitize_terminal_inline(&node.status)),
                status_style,
            );

            if component.roots.contains(&node.id) {
                content.append_styled(" (root)", theme.dimmed.clone());
            } else if let Some(parents) = parent_map.get(&node.id) {
                content.append_styled(&format_parent_list(Some(parents)), theme.dimmed.clone());
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
        "No open/in_progress/blocked issues found",
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
                },
                GraphNode {
                    id: "bd-002".to_string(),
                    title: "Child 1".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
                },
            ],
            edges: vec![("bd-002".to_string(), "bd-001".to_string())],
        };

        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"root\":\"bd-001\""));
        assert!(json.contains("\"count\":3"));
    }

    #[test]
    fn graph_human_id_helpers_escape_terminal_controls() {
        let ids = vec![
            "bd-root\x1b[2J".to_string(),
            "bd-child\x07".to_string(),
            "bd-next\rline".to_string(),
        ];

        let rendered_ids = format_id_list(&ids);
        let rendered_parents = format_parent_list(Some(&ids));
        let rendered_compact = format_compact_dependency_edges(
            "bd-root\x1b[2J",
            &[("bd-child\x07".to_string(), "bd-root\x1b[2J".to_string())],
        );

        for rendered in [rendered_ids, rendered_parents, rendered_compact] {
            assert!(!rendered.chars().any(char::is_control));
            assert!(rendered.contains("\\u{1b}[2J"));
            assert!(rendered.contains("\\u{7}") || rendered.contains("\\r"));
        }
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
                    },
                    GraphNode {
                        id: "bd-002".to_string(),
                        title: "Child Issue".to_string(),
                        status: "blocked".to_string(),
                        priority: 2,
                        depth: 1,
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
                },
                GraphNode {
                    id: "bd-002".to_string(),
                    title: "Root 2".to_string(),
                    status: "open".to_string(),
                    priority: 2,
                    depth: 0,
                },
                GraphNode {
                    id: "bd-003".to_string(),
                    title: "Shared Child".to_string(),
                    status: "blocked".to_string(),
                    priority: 3,
                    depth: 1,
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
                },
                GraphNode {
                    id: "bd-a".to_string(),
                    title: "A".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
                },
                GraphNode {
                    id: "bd-b".to_string(),
                    title: "B".to_string(),
                    status: "blocked".to_string(),
                    priority: 1,
                    depth: 1,
                },
                GraphNode {
                    id: "bd-c".to_string(),
                    title: "C".to_string(),
                    status: "blocked".to_string(),
                    priority: 2,
                    depth: 2,
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
                        },
                        GraphNode {
                            id: "comp2-b".to_string(),
                            title: "Comp2 Child".to_string(),
                            status: "blocked".to_string(),
                            priority: 2,
                            depth: 1,
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
            };

            let json = serde_json::to_string(&node).unwrap();
            assert!(json.contains(&format!("\"status\":\"{status}\"")));
        }
    }

    #[test]
    fn test_calculate_depths_cycle_has_no_roots() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        let a = Issue {
            id: "bd-a".to_string(),
            title: "A".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };
        let b = Issue {
            id: "bd-b".to_string(),
            title: "B".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };

        storage.create_issue(&a, "test").unwrap();
        storage.create_issue(&b, "test").unwrap();
        let created_at = t1.to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-a', 'bd-b', 'waits-for', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-b', 'bd-a', 'waits-for', '{created_at}', 'test');"
            ))
            .unwrap();

        let all_dependencies = storage.get_all_dependency_records().unwrap();
        let nodes = vec!["bd-a".to_string(), "bd-b".to_string()];
        let component_set: HashSet<&str> = nodes.iter().map(String::as_str).collect();

        let (depths, roots) = calculate_depths(&all_dependencies, &nodes, &component_set);
        assert!(roots.is_empty());
        assert_eq!(depths.get("bd-a"), Some(&0));
        assert_eq!(depths.get("bd-b"), Some(&0));
    }

    #[test]
    fn test_graph_all_cycle_robustness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        // IssueValidator requires the prefix-hash ID format, so use
        // bd-root / bd-1 / bd-2 rather than the bare "root" the test
        // historically used.
        let root = Issue {
            id: "bd-root".to_string(),
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

        let created_at = t1.to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-1', 'bd-root', 'waits-for', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-2', 'bd-1', 'waits-for', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-1', 'bd-2', 'waits-for', '{created_at}', 'test');"
            ))
            .unwrap();

        let ctx = OutputContext::from_flags(true, false, true); // JSON mode

        // This should not hang even with root feeding into cycle
        // If it hangs, the test runner will timeout
        let result = graph_all(&storage, false, false, &ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_collect_single_graph_preserves_missing_dependent_placeholder() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        let root = Issue {
            id: "bd-root".to_string(),
            title: "Root".to_string(),
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type: crate::model::IssueType::Task,
            created_at: t1,
            updated_at: t1,
            ..Default::default()
        };

        storage.create_issue(&root, "test").unwrap();

        let created_at = t1.to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-missing', 'bd-root', 'blocks', '{created_at}', 'test');
                 PRAGMA foreign_keys = ON;"
            ))
            .unwrap();

        let traversal = collect_single_graph(&storage, "bd-root", &root)
            .expect("graph traversal should preserve missing dependent placeholders");

        let missing = traversal
            .issues_by_id
            .get("bd-missing")
            .expect("missing dependent should be represented in graph output");
        assert_eq!(missing.title, "[missing issue: bd-missing]");
        assert_eq!(missing.status, Status::Tombstone);
        assert!(
            traversal
                .edges
                .contains(&(String::from("bd-missing"), String::from("bd-root"))),
            "graph should retain the dangling edge"
        );
    }

    #[test]
    fn test_collect_single_graph_uses_first_visit_dfs_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        for (id, title) in [
            ("bd-root", "Root"),
            ("bd-a", "A"),
            ("bd-b", "B"),
            ("bd-c", "C"),
        ] {
            let issue = Issue {
                id: id.to_string(),
                title: title.to_string(),
                status: Status::Open,
                priority: crate::model::Priority::MEDIUM,
                issue_type: crate::model::IssueType::Task,
                created_at: t1,
                updated_at: t1,
                ..Default::default()
            };
            storage.create_issue(&issue, "test").unwrap();
        }

        let created_at = t1.to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-a', 'bd-root', 'blocks', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-b', 'bd-root', 'blocks', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-c', 'bd-a', 'blocks', '{created_at}', 'test');"
            ))
            .unwrap();

        let root = storage
            .get_issue("bd-root")
            .unwrap()
            .expect("root issue should exist");
        let traversal =
            collect_single_graph(&storage, "bd-root", &root).expect("graph traversal should work");

        assert_eq!(
            traversal.traversal_order,
            vec![
                "bd-root".to_string(),
                "bd-a".to_string(),
                "bd-c".to_string(),
                "bd-b".to_string()
            ]
        );
    }

    #[test]
    fn test_graph_parent_child_edges_use_blocker_direction() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        for (id, title) in [("bd-parent", "Parent"), ("bd-child", "Child")] {
            let issue = Issue {
                id: id.to_string(),
                title: title.to_string(),
                status: Status::Open,
                priority: crate::model::Priority::MEDIUM,
                issue_type: crate::model::IssueType::Task,
                created_at: t1,
                updated_at: t1,
                ..Default::default()
            };
            storage.create_issue(&issue, "test").unwrap();
        }

        storage
            .add_dependency("bd-child", "bd-parent", "parent-child", "tester")
            .unwrap();

        let child = storage
            .get_issue("bd-child")
            .unwrap()
            .expect("child issue should exist");
        let child_traversal = collect_single_graph(&storage, "bd-child", &child)
            .expect("child graph traversal should work");
        assert_eq!(
            child_traversal.traversal_order,
            vec!["bd-child".to_string(), "bd-parent".to_string()]
        );
        assert_eq!(
            child_traversal.edges,
            vec![("bd-parent".to_string(), "bd-child".to_string())],
            "parent-child rows are stored child -> parent but block parent -> child"
        );

        let parent = storage
            .get_issue("bd-parent")
            .unwrap()
            .expect("parent issue should exist");
        let parent_traversal = collect_single_graph(&storage, "bd-parent", &parent)
            .expect("parent graph traversal should work");
        assert_eq!(
            parent_traversal.traversal_order,
            vec!["bd-parent".to_string()]
        );
        assert!(
            parent_traversal.edges.is_empty(),
            "a child is not a dependent of its parent in the blocker graph"
        );

        let all_dependencies = storage.get_all_dependency_records().unwrap();
        let nodes = vec!["bd-parent".to_string(), "bd-child".to_string()];
        let component_set: HashSet<&str> = nodes.iter().map(String::as_str).collect();
        let dependency_edges =
            dependency_edges_for_component(&all_dependencies, &nodes, &component_set);
        assert_eq!(
            dependency_edges,
            vec![("bd-parent".to_string(), "bd-child".to_string())]
        );

        let (depths, roots) = calculate_depths(&all_dependencies, &nodes, &component_set);
        assert_eq!(roots, vec!["bd-child".to_string()]);
        assert_eq!(depths.get("bd-child"), Some(&0));
        assert_eq!(depths.get("bd-parent"), Some(&1));
    }

    #[test]
    fn test_cache_graph_dependents_batches_frontier_roots() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = chrono::Utc::now();

        for (id, title, priority) in [
            ("bd-root", "Root", 1),
            ("bd-a", "A", 1),
            ("bd-b", "B", 2),
            ("bd-c", "C", 3),
        ] {
            let issue = Issue {
                id: id.to_string(),
                title: title.to_string(),
                status: Status::Open,
                priority: crate::model::Priority(priority),
                issue_type: crate::model::IssueType::Task,
                created_at: t1,
                updated_at: t1,
                ..Default::default()
            };
            storage.create_issue(&issue, "test").unwrap();
        }

        let created_at = t1.to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-a', 'bd-root', 'blocks', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-b', 'bd-root', 'blocks', '{created_at}', 'test');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-c', 'bd-a', 'blocks', '{created_at}', 'test');"
            ))
            .unwrap();

        let mut cache = HashMap::new();
        cache_graph_dependents(
            &storage,
            &mut cache,
            &["bd-root".to_string(), "bd-a".to_string()],
        )
        .unwrap();

        let root_dependents: Vec<_> = cache.get("bd-root").map_or_else(Vec::new, |dependents| {
            dependents
                .iter()
                .map(|dependent| dependent.id.as_str())
                .collect()
        });
        assert_eq!(root_dependents, vec!["bd-a", "bd-b"]);

        let a_dependents: Vec<_> = cache.get("bd-a").map_or_else(Vec::new, |dependents| {
            dependents
                .iter()
                .map(|dependent| dependent.id.as_str())
                .collect()
        });
        assert_eq!(a_dependents, vec!["bd-c"]);
    }

    #[test]
    fn test_calculate_depths_from_dependency_edges_uses_longest_path() {
        let nodes = vec![
            "root".to_string(),
            "bd-a".to_string(),
            "bd-b".to_string(),
            "bd-c".to_string(),
            "bd-d".to_string(),
        ];
        let dependency_edges = vec![
            ("bd-a".to_string(), "root".to_string()),
            ("bd-b".to_string(), "root".to_string()),
            ("bd-c".to_string(), "bd-b".to_string()),
            ("bd-d".to_string(), "bd-a".to_string()),
            ("bd-d".to_string(), "bd-c".to_string()),
        ];
        let root_nodes = ["root".to_string()];

        let depths = calculate_depths_from_dependency_edges(&nodes, &dependency_edges, &root_nodes);

        assert_eq!(depths.get("root"), Some(&0));
        assert_eq!(depths.get("bd-a"), Some(&1));
        assert_eq!(depths.get("bd-b"), Some(&1));
        assert_eq!(depths.get("bd-c"), Some(&2));
        assert_eq!(depths.get("bd-d"), Some(&3));
    }

    #[test]
    fn test_calculate_depths_from_dependency_edges_collapses_reachable_cycle() {
        let nodes = vec!["root".to_string(), "bd-a".to_string(), "bd-b".to_string()];
        let dependency_edges = vec![
            ("bd-a".to_string(), "root".to_string()),
            ("bd-b".to_string(), "bd-a".to_string()),
            ("bd-a".to_string(), "bd-b".to_string()),
        ];
        let root_nodes = ["root".to_string()];

        let depths = calculate_depths_from_dependency_edges(&nodes, &dependency_edges, &root_nodes);

        assert_eq!(depths.get("root"), Some(&0));
        assert_eq!(depths.get("bd-a"), Some(&1));
        assert_eq!(depths.get("bd-b"), Some(&1));
    }

    #[test]
    fn test_calculate_depths_from_dependency_edges_tolerates_external_edge_endpoint() {
        let nodes = vec!["root".to_string(), "bd-a".to_string()];
        let dependency_edges = vec![
            ("bd-a".to_string(), "root".to_string()),
            ("bd-a".to_string(), "bd-missing".to_string()),
            ("bd-unknown".to_string(), "root".to_string()),
        ];
        let root_nodes = ["root".to_string()];

        let depths = calculate_depths_from_dependency_edges(&nodes, &dependency_edges, &root_nodes);

        assert_eq!(depths.get("root"), Some(&0));
        assert_eq!(depths.get("bd-a"), Some(&1));
        assert!(!depths.contains_key("bd-missing"));
        assert!(!depths.contains_key("bd-unknown"));
    }

    #[test]
    fn test_format_compact_dependency_edges_preserves_branching() {
        let edges = vec![
            ("bd-a".to_string(), "root".to_string()),
            ("bd-b".to_string(), "root".to_string()),
            ("bd-c".to_string(), "bd-a".to_string()),
            ("bd-c".to_string(), "bd-b".to_string()),
        ];

        let compact = format_compact_dependency_edges("root", &edges);
        assert_eq!(compact, "root <- bd-a, bd-b; bd-a <- bd-c; bd-b <- bd-c");
    }

    #[test]
    fn test_human_graph_render_mode_prefers_compact_over_rich() {
        assert_eq!(
            human_graph_render_mode(OutputMode::Rich, true),
            HumanGraphRenderMode::Compact
        );
        assert_eq!(
            human_graph_render_mode(OutputMode::Rich, false),
            HumanGraphRenderMode::Rich
        );
        assert_eq!(
            human_graph_render_mode(OutputMode::Plain, true),
            HumanGraphRenderMode::Compact
        );
        assert_eq!(
            human_graph_render_mode(OutputMode::Plain, false),
            HumanGraphRenderMode::Plain
        );
    }

    #[test]
    fn test_sort_single_graph_nodes_keeps_root_first_without_reordering_subtrees() {
        let mut nodes = vec![
            GraphNode {
                id: "branch-a".to_string(),
                title: "branch-a".to_string(),
                status: "open".to_string(),
                priority: 1,
                depth: 1,
            },
            GraphNode {
                id: "middle".to_string(),
                title: "middle".to_string(),
                status: "open".to_string(),
                priority: 0,
                depth: 2,
            },
            GraphNode {
                id: "leaf".to_string(),
                title: "leaf".to_string(),
                status: "open".to_string(),
                priority: 3,
                depth: 3,
            },
            GraphNode {
                id: "branch-b".to_string(),
                title: "branch-b".to_string(),
                status: "open".to_string(),
                priority: 2,
                depth: 1,
            },
            GraphNode {
                id: "root".to_string(),
                title: "root".to_string(),
                status: "open".to_string(),
                priority: 4,
                depth: 0,
            },
        ];

        sort_single_graph_nodes(&mut nodes, "root");

        let ordered_ids: Vec<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(
            ordered_ids,
            vec!["root", "branch-a", "middle", "leaf", "branch-b"]
        );
    }

    #[test]
    fn test_format_single_graph_dot_emits_valid_dot() {
        let nodes = vec![
            GraphNode {
                id: "bd-001".to_string(),
                title: "Root \"issue\"".to_string(),
                status: "open".to_string(),
                priority: 0,
                depth: 0,
            },
            GraphNode {
                id: "bd-002".to_string(),
                title: "Dependent".to_string(),
                status: "blocked".to_string(),
                priority: 2,
                depth: 1,
            },
        ];
        let edges = vec![("bd-002".to_string(), "bd-001".to_string())];

        let dot = format_single_graph_dot(&nodes, &edges, "bd-001");

        // Structural well-formedness.
        assert!(dot.starts_with("digraph dependencies {"));
        assert!(dot.trim_end().ends_with('}'));
        assert_eq!(
            dot.matches('{').count(),
            dot.matches('}').count(),
            "DOT braces must be balanced: {dot}"
        );

        // Both nodes declared and the dependency edge present.
        assert!(dot.contains("\"bd-001\" [label="));
        assert!(dot.contains("\"bd-002\" [label="));
        assert!(dot.contains("\"bd-002\" -> \"bd-001\";"));

        // Root carries the heavier border; status drives fill color.
        assert!(dot.contains("penwidth=2"));
        assert!(dot.contains("fillcolor=\"lightblue\""));
        assert!(dot.contains("fillcolor=\"lightcoral\""));

        // Quotes inside titles are escaped, never emitted raw.
        assert!(dot.contains("Root \\\"issue\\\""));
        assert!(!dot.contains("Root \"issue\""));
    }

    #[test]
    fn test_format_single_graph_dot_root_only_is_valid() {
        let nodes = vec![GraphNode {
            id: "bd-001".to_string(),
            title: "Lonely root".to_string(),
            status: "open".to_string(),
            priority: 1,
            depth: 0,
        }];

        let dot = format_single_graph_dot(&nodes, &[], "bd-001");

        assert!(dot.starts_with("digraph dependencies {"));
        assert_eq!(dot.matches('{').count(), dot.matches('}').count());
        assert!(dot.contains("\"bd-001\" [label="));
        assert!(!dot.contains("->"));
    }

    #[test]
    fn test_format_all_graph_dot_clusters_components() {
        let components = vec![
            ConnectedComponent {
                nodes: vec![
                    GraphNode {
                        id: "bd-001".to_string(),
                        title: "Root".to_string(),
                        status: "open".to_string(),
                        priority: 1,
                        depth: 0,
                    },
                    GraphNode {
                        id: "bd-002".to_string(),
                        title: "Child".to_string(),
                        status: "blocked".to_string(),
                        priority: 2,
                        depth: 1,
                    },
                ],
                edges: vec![("bd-002".to_string(), "bd-001".to_string())],
                roots: vec!["bd-001".to_string()],
            },
            ConnectedComponent {
                nodes: vec![GraphNode {
                    id: "bd-003".to_string(),
                    title: "Solo".to_string(),
                    status: "open".to_string(),
                    priority: 0,
                    depth: 0,
                }],
                edges: vec![],
                roots: vec!["bd-003".to_string()],
            },
        ];

        let dot = format_all_graph_dot(&components);

        assert!(dot.starts_with("digraph dependencies {"));
        assert_eq!(
            dot.matches('{').count(),
            dot.matches('}').count(),
            "DOT braces must be balanced: {dot}"
        );
        assert!(dot.contains("subgraph cluster_0 {"));
        assert!(dot.contains("subgraph cluster_1 {"));
        assert!(dot.contains("label=\"Component 1\";"));
        assert!(dot.contains("label=\"Component 2\";"));
        assert!(dot.contains("\"bd-002\" -> \"bd-001\";"));
        assert!(dot.contains("\"bd-003\" [label="));
    }

    #[test]
    fn test_format_all_graph_dot_empty_is_valid() {
        let dot = format_all_graph_dot(&[]);
        assert!(dot.starts_with("digraph dependencies {"));
        assert!(dot.trim_end().ends_with('}'));
        assert_eq!(dot.matches('{').count(), dot.matches('}').count());
    }

    #[test]
    fn test_sanitize_dot_string_escapes_metacharacters_and_controls() {
        let escaped = sanitize_dot_string("a\"b\\c\x1b[2J");
        assert!(!escaped.chars().any(char::is_control));
        assert!(escaped.contains("\\\""), "quote must be escaped: {escaped}");
        assert!(
            !escaped.contains("a\"b"),
            "raw quote must not survive: {escaped}"
        );
    }
}
