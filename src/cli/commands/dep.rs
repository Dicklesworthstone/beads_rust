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
use std::collections::HashSet;
use std::path::Path;

/// Execute the dep command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(command: &DepCommands, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;

    let actor = config::resolve_actor(&config_layer);

    match command {
        DepCommands::Add(args) => dep_add(args, &mut storage, &resolver, &all_ids, &actor, json),
        DepCommands::Remove(args) => {
            dep_remove(args, &mut storage, &resolver, &all_ids, &actor, json)
        }
        DepCommands::List(args) => dep_list(args, &storage, &resolver, &all_ids, json),
        DepCommands::Tree(args) => dep_tree(args, &storage, &resolver, &all_ids, json),
        DepCommands::Cycles(args) => dep_cycles(args, &storage, json),
    }
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
    let dep_type: DependencyType = dep_type_str.parse().unwrap_or_else(|_| {
        eprintln!("Warning: Unknown dependency type '{dep_type_str}', using 'blocks'");
        DependencyType::Blocks
    });

    // Self-dependency check
    if issue_id == depends_on_id {
        return Err(BeadsError::SelfDependency { id: issue_id });
    }

    // Cycle check for blocking types only
    if dep_type.is_blocking()
        && !depends_on_id.starts_with("external:")
        && storage.would_create_cycle(&issue_id, &depends_on_id)?
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

fn dep_tree(
    args: &DepTreeArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
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
    }

    let mut nodes = Vec::new();
    let mut visited = HashSet::new();

    let mut queue = vec![QueueItem {
        id: root_id.clone(),
        depth: 0,
        parent_id: None,
    }];

    while let Some(item) = queue.pop() {
        if visited.contains(&item.id) {
            continue;
        }
        visited.insert(item.id.clone());

        let issue = if item.id == root_id {
            Some(root_issue.clone())
        } else {
            storage.get_issue(&item.id)?
        };

        let (title, priority, status) = if let Some(ref issue) = issue {
            (
                issue.title.clone(),
                issue.priority.0,
                issue.status.as_str().to_string(),
            )
        } else {
            // External or missing issue
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
        if item.depth < args.max_depth {
            // Get dependents (issues that depend on this one)
            let dependents = storage.get_dependents(&item.id)?;
            for dep_id in dependents {
                if !visited.contains(&dep_id) {
                    queue.push(QueueItem {
                        id: dep_id,
                        depth: item.depth + 1,
                        parent_id: Some(item.id.clone()),
                    });
                }
            }
        }
    }

    // Sort by depth, then priority, then id
    nodes.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });

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
        let would_cycle = storage.would_create_cycle("bd-002", "bd-001").unwrap();
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
        let would_cycle = storage.would_create_cycle("bd-003", "bd-001").unwrap();
        assert!(would_cycle);

        // bd-003 -> bd-002 would also create a cycle
        let would_cycle = storage.would_create_cycle("bd-003", "bd-002").unwrap();
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
        let would_cycle = storage.would_create_cycle("bd-003", "bd-002").unwrap();
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
    fn test_dep_direction_variants() {
        assert!(matches!(DepDirection::Down, DepDirection::Down));
        assert!(matches!(DepDirection::Up, DepDirection::Up));
        assert!(matches!(DepDirection::Both, DepDirection::Both));
    }
}
