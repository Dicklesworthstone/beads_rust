//! Dependency command implementation.

use crate::cli::{
    DepAddArgs, DepCommands, DepCyclesArgs, DepDirection, DepListArgs, DepRemoveArgs, DepTreeArgs,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::DependencyType;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Execute the dep command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(command: &DepCommands, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let storage = &mut storage_ctx.storage;

    let actor = config::resolve_actor(&config_layer);

    let external_db_paths = config::external_project_db_paths(&config_layer, &beads_dir);

    match command {
        DepCommands::Add(args) => dep_add(args, storage, &resolver, &all_ids, &actor, json),
        DepCommands::Remove(args) => dep_remove(args, storage, &resolver, &all_ids, &actor, json),
        DepCommands::List(args) => {
            dep_list(args, storage, &resolver, &all_ids, &external_db_paths, json)
        }
        DepCommands::Tree(args) => {
            dep_tree(args, storage, &resolver, &all_ids, &external_db_paths, json)
        }
        DepCommands::Cycles(args) => dep_cycles(args, storage, json),
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
    json: bool,
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

    if json {
        let result = DepActionResult {
            status: if added { "ok" } else { "exists" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type: dep_type.as_str().to_string(),
            action: if added { "added" } else { "already_exists" }.to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if added {
        println!(
            "Added dependency: {} -> {} ({})",
            issue_id,
            depends_on_id,
            dep_type.as_str()
        );
    } else {
        println!("Dependency already exists: {issue_id} -> {depends_on_id}");
    }

    Ok(())
}

fn dep_remove(
    args: &DepRemoveArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
    json: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, &args.issue)?;

    // External dependencies don't need resolution
    let depends_on_id = if args.depends_on.starts_with("external:") {
        args.depends_on.clone()
    } else {
        resolve_issue_id(storage, resolver, all_ids, &args.depends_on)?
    };

    let removed = storage.remove_dependency(&issue_id, &depends_on_id, actor)?;

    if json {
        let result = DepActionResult {
            status: if removed { "ok" } else { "not_found" }.to_string(),
            issue_id: issue_id.clone(),
            depends_on_id: depends_on_id.clone(),
            dep_type: "unknown".to_string(),
            action: if removed { "removed" } else { "not_found" }.to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if removed {
        println!("Removed dependency: {issue_id} -> {depends_on_id}");
    } else {
        println!("Dependency not found: {issue_id} -> {depends_on_id}");
    }

    Ok(())
}

fn dep_list(
    args: &DepListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    external_db_paths: &HashMap<String, PathBuf>,
    json: bool,
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

    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if items.is_empty() {
        let direction_str = match args.direction {
            DepDirection::Down => "dependencies",
            DepDirection::Up => "dependents",
            DepDirection::Both => "dependencies or dependents",
        };
        println!("No {direction_str} for {issue_id}");
        return Ok(());
    }

    let header = match args.direction {
        DepDirection::Down => format!("Dependencies of {} ({}):", issue_id, items.len()),
        DepDirection::Up => format!("Dependents of {} ({}):", issue_id, items.len()),
        DepDirection::Both => format!(
            "Dependencies and dependents of {} ({}):",
            issue_id,
            items.len()
        ),
    };
    println!("{header}");

    for item in items {
        let arrow = if item.issue_id == issue_id {
            format!("  -> {} ({})", item.depends_on_id, item.dep_type)
        } else {
            format!("  <- {} ({})", item.issue_id, item.dep_type)
        };
        println!(
            "{}: {} [P{}] [{}]",
            arrow, item.title, item.priority, item.status
        );
    }

    Ok(())
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
    json: bool,
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

    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }

    // Text tree output
    if nodes.is_empty() {
        println!("No dependency tree for {root_id}");
        return Ok(());
    }

    for node in &nodes {
        let indent = "  ".repeat(node.depth);
        let prefix = if node.depth == 0 {
            ""
        } else if node.truncated {
            "├── (truncated) "
        } else {
            "├── "
        };
        println!(
            "{}{}{}: {} [P{}] [{}]",
            indent, prefix, node.id, node.title, node.priority, node.status
        );
    }

    Ok(())
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

fn dep_cycles(_args: &DepCyclesArgs, storage: &SqliteStorage, json: bool) -> Result<()> {
    let cycles = storage.detect_all_cycles()?;
    let count = cycles.len();

    if json {
        let result = CyclesResult { cycles, count };
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    if count == 0 {
        println!("No dependency cycles detected.");
    } else {
        println!("Found {count} dependency cycle(s):");
        for (i, cycle) in cycles.iter().enumerate() {
            println!("  {}. {}", i + 1, cycle.join(" -> "));
        }
    }

    Ok(())
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
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

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
    }

    #[test]
    fn test_blocking_dependency_types() {
        assert!(DependencyType::Blocks.is_blocking());
        assert!(DependencyType::ParentChild.is_blocking());
        assert!(!DependencyType::Related.is_blocking());
        assert!(!DependencyType::Duplicates.is_blocking());
    }

    #[test]
    fn test_add_dependency() {
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
    }

    #[test]
    fn test_remove_dependency() {
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
    }

    #[test]
    fn test_get_dependencies() {
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
    }

    #[test]
    fn test_get_dependents() {
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
    }

    #[test]
    fn test_cycle_detection_simple() {
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
    }

    #[test]
    fn test_cycle_detection_transitive() {
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
    }

    #[test]
    fn test_no_false_positive_cycle() {
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
    }

    #[test]
    fn test_dep_action_result_json() {
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
    }

    #[test]
    fn test_dep_list_item_json() {
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
    }

    #[test]
    fn test_cycles_result_json() {
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
    }

    #[test]
    fn test_external_dependency_prefix_check() {
        let external = "external:jira-123";
        assert!(external.starts_with("external:"));

        let normal = "bd-001";
        assert!(!normal.starts_with("external:"));
    }

    #[test]
    fn test_dep_direction_default() {
        let direction = DepDirection::default();
        assert_eq!(direction, DepDirection::Down);
    }

    #[test]
    fn test_apply_external_dep_list_metadata_sets_status_and_title() {
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
    }

    #[test]
    fn test_apply_external_dep_list_metadata_preserves_title() {
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
    }

    #[test]
    fn test_apply_external_dep_list_metadata_external_issue_id() {
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
    }

    #[test]
    fn test_dep_direction_variants() {
        assert!(matches!(DepDirection::Down, DepDirection::Down));
        assert!(matches!(DepDirection::Up, DepDirection::Up));
        assert!(matches!(DepDirection::Both, DepDirection::Both));
    }
}
