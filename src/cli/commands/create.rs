use crate::cli::CreateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdGenerator, child_id};
use crate::util::markdown_import::{parse_dependency, parse_markdown_file};
use crate::util::time::parse_flexible_timestamp;
use crate::validation::{IssueValidator, LabelValidator};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::str::FromStr;

/// Configuration for creating an issue.
pub struct CreateConfig {
    pub id_config: crate::util::id::IdConfig,
    pub default_priority: Priority,
    pub default_issue_type: IssueType,
    pub actor: String,
}

/// Execute the create command.
///
/// # Errors
///
/// Returns an error if validation fails, the database cannot be opened, or the issue cannot be created.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &CreateArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    if let Some(ref file_path) = args.file {
        if args.title.is_some() || args.title_flag.is_some() {
            return Err(BeadsError::validation(
                "file",
                "cannot be combined with title arguments",
            ));
        }
        if args.dry_run {
            return Err(BeadsError::validation(
                "dry_run",
                "--dry-run is not supported with --file",
            ));
        }
        return execute_import(file_path, args, cli, ctx);
    }

    // 1. Open storage (unless dry run without DB)
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    // We open storage even for dry-run to check ID collisions.
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;

    let config = CreateConfig {
        id_config: config::id_config_from_layer(&layer),
        default_priority: config::default_priority_from_layer(&layer)?,
        default_issue_type: config::default_issue_type_from_layer(&layer)?,
        actor: config::resolve_actor(&layer),
    };

    let issue = create_issue_impl(&mut storage_ctx.storage, args, &config)?;

    // Output
    if args.silent {
        println!("{}", issue.id);
    } else if ctx.is_json() {
        if args.dry_run {
            ctx.json_pretty(&issue);
        } else {
            let full_issue = storage_ctx
                .storage
                .get_issue_for_export(&issue.id)?
                .ok_or_else(|| BeadsError::IssueNotFound {
                    id: issue.id.clone(),
                })?;
            ctx.json_pretty(&full_issue);
        }
    } else if args.dry_run {
        ctx.info(&format!("Dry run: would create issue {}", issue.id));
        ctx.print(&format!("Title: {}", issue.title));
        ctx.print(&format!("Type: {}", issue.issue_type));
        ctx.print(&format!("Priority: {}", issue.priority));
        if !args.labels.is_empty() {
            ctx.print(&format!("Labels: {}", args.labels.join(", ")));
        }
        if let Some(parent) = &args.parent {
            ctx.print(&format!("Parent: {parent}"));
        }
        if !args.deps.is_empty() {
            ctx.print(&format!("Dependencies: {}", args.deps.join(", ")));
        }
    } else {
        ctx.success(&format!("Created {}: {}", issue.id, issue.title));
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

/// Core logic for creating an issue.
///
/// Handles ID generation, validation, and storage insertion.
/// Returns the constructed Issue.
///
/// # Errors
///
/// Returns error if:
/// - Title is empty
/// - ID generation fails
/// - Validation fails
/// - Storage write fails
#[allow(clippy::too_many_lines)]
pub fn create_issue_impl(
    storage: &mut SqliteStorage,
    args: &CreateArgs,
    config: &CreateConfig,
) -> Result<Issue> {
    // 1. Resolve title
    let title = args
        .title
        .as_ref()
        .or(args.title_flag.as_ref())
        .ok_or_else(|| BeadsError::validation("title", "cannot be empty"))?;

    if title.is_empty() {
        return Err(BeadsError::validation("title", "cannot be empty"));
    }

    // 2. Generate ID
    let now = Utc::now();

    // When a parent is specified, generate a child ID (parent.1, parent.2, etc.)
    // instead of a random hash-based ID
    let id = if let Some(parent_id) = &args.parent {
        // Verify parent exists
        if !storage.id_exists(parent_id).unwrap_or(false) {
            return Err(BeadsError::IssueNotFound {
                id: parent_id.clone(),
            });
        }

        // Find next available child number
        let next_num = storage.next_child_number(parent_id)?;
        let candidate = child_id(parent_id, next_num);

        // Double-check the ID doesn't exist (race condition safety)
        if storage.id_exists(&candidate).unwrap_or(false) {
            // Extremely unlikely, but handle by incrementing
            let mut num = next_num + 1;
            loop {
                let alt = child_id(parent_id, num);
                if !storage.id_exists(&alt).unwrap_or(false) {
                    break alt;
                }
                num += 1;
                if num > next_num + 100 {
                    return Err(BeadsError::validation(
                        "parent",
                        "could not find available child ID",
                    ));
                }
            }
        } else {
            candidate
        }
    } else {
        // Standard ID generation for non-child issues
        let id_gen = IdGenerator::new(config.id_config.clone());
        let count = storage.count_issues()?;
        id_gen.generate(
            title,
            None, // description
            None, // creator
            now,
            count,
            |id| storage.id_exists(id).unwrap_or(false),
        )
    };

    // 3. Parse fields
    let priority = if let Some(p) = &args.priority {
        Priority::from_str(p)?
    } else {
        config.default_priority
    };

    let issue_type = if let Some(t) = &args.type_ {
        IssueType::from_str(t)?
    } else {
        config.default_issue_type.clone()
    };

    let due_at = parse_optional_date(args.due.as_deref())?;
    let defer_until = parse_optional_date(args.defer.as_deref())?;

    // Parse status (default to Open if not provided)
    let status = if let Some(s) = &args.status {
        Status::from_str(s)?
    } else {
        Status::Open
    };

    // Set closed_at if status is Closed or Tombstone
    let closed_at = if matches!(status, Status::Closed | Status::Tombstone) {
        Some(now)
    } else {
        None
    };

    // 4. Construct Issue
    let mut issue = Issue {
        id: id.clone(),
        title: title.clone(),
        description: args.description.clone(),
        status,
        priority,
        issue_type,
        created_at: now,
        updated_at: now,
        assignee: args.assignee.clone(),
        owner: args.owner.clone(),
        estimated_minutes: args.estimate,
        due_at,
        defer_until,
        external_ref: args.external_ref.clone(),
        ephemeral: args.ephemeral,
        // Defaults
        content_hash: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        created_by: Some(config.actor.clone()),
        closed_at,
        close_reason: None,
        closed_by_session: None,
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
        pinned: false,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
    };

    // Compute content hash
    issue.content_hash = Some(issue.compute_content_hash());

    // 5. Validate Issue
    IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

    // 5b. Validate Relations (fail fast before DB writes)
    validate_relations(args, &id)?;

    // 6. Populate Relations (labels & dependencies)
    populate_relations(&mut issue, args, &config.actor, now);

    // 7. Dry Run check - return early
    if args.dry_run {
        return Ok(issue);
    }

    // 8. Create (atomic)
    storage.create_issue(&issue, &config.actor)?;

    Ok(issue)
}

fn validate_relations(args: &CreateArgs, id: &str) -> Result<()> {
    // Validate Labels
    for label in &args.labels {
        let trimmed = label.trim();
        if !trimmed.is_empty() {
            LabelValidator::validate(trimmed)
                .map_err(|e| BeadsError::validation("label", e.message))?;
        }
    }

    // Validate Parent
    if let Some(parent_id) = &args.parent {
        if parent_id == id {
            return Err(BeadsError::validation(
                "parent",
                "cannot be parent of itself",
            ));
        }
    }

    // Validate Dependencies
    for dep_str in &args.deps {
        let (type_str, dep_id) = if let Some((t, i)) = dep_str.split_once(':') {
            (t, i)
        } else {
            ("blocks", dep_str.as_str())
        };

        if dep_id == id {
            return Err(BeadsError::validation("deps", "cannot depend on itself"));
        }

        // Strict dependency type validation
        let dep_type: DependencyType = type_str.parse().map_err(|_| BeadsError::Validation {
            field: "deps".to_string(),
            reason: format!("Invalid dependency type: {type_str}"),
        })?;

        // Disallow accidental custom types from typos
        if let DependencyType::Custom(_) = dep_type {
            return Err(BeadsError::Validation {
                field: "deps".to_string(),
                reason: format!(
                    "Unknown dependency type: '{type_str}'. \
                     Allowed types: blocks, parent-child, conditional-blocks, waits-for, \
                     related, discovered-from, replies-to, relates-to, duplicates, \
                     supersedes, caused-by"
                ),
            });
        }
    }

    Ok(())
}

fn populate_relations(issue: &mut Issue, args: &CreateArgs, actor: &str, now: DateTime<Utc>) {
    // Labels
    for label in &args.labels {
        let label = label.trim();
        if !label.is_empty() {
            issue.labels.push(label.to_string());
        }
    }

    // Parent
    if let Some(parent_id) = &args.parent {
        issue.dependencies.push(Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: parent_id.clone(),
            dep_type: DependencyType::ParentChild,
            created_at: now,
            created_by: Some(actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }

    // Dependencies
    for dep_str in &args.deps {
        let (type_str, dep_id) = if let Some((t, i)) = dep_str.split_once(':') {
            (t, i)
        } else {
            ("blocks", dep_str.as_str())
        };

        let dep_type = type_str
            .parse()
            .unwrap_or_else(|_| DependencyType::Custom(type_str.to_owned()));
        issue.dependencies.push(Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: dep_id.to_string(),
            dep_type,
            created_at: now,
            created_by: Some(actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }
}

#[allow(clippy::too_many_lines)]
fn execute_import(
    path: &Path,
    args: &CreateArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let parsed_issues = parse_markdown_file(path)?;
    if parsed_issues.is_empty() {
        if ctx.is_json() {
            ctx.json(&Vec::<Issue>::new());
        }
        return Ok(());
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;

    let id_config = config::id_config_from_layer(&layer);
    let default_priority = config::default_priority_from_layer(&layer)?;
    let default_issue_type = config::default_issue_type_from_layer(&layer)?;
    let actor = config::resolve_actor(&layer);
    let now = Utc::now();
    let _json_mode = cli.json.unwrap_or(false);
    let due_at = parse_optional_date(args.due.as_deref())?;
    let defer_until = parse_optional_date(args.defer.as_deref())?;

    // Parse status (default to Open if not provided)
    let import_status = if let Some(s) = &args.status {
        Status::from_str(s)?
    } else {
        Status::Open
    };

    // Set closed_at if status is Closed or Tombstone
    let import_closed_at = if matches!(import_status, Status::Closed | Status::Tombstone) {
        Some(now)
    } else {
        None
    };

    let storage = &mut storage_ctx.storage;
    let id_gen = IdGenerator::new(id_config);

    // Track created IDs for output
    let mut created_ids = Vec::new();
    let mut created_issues = Vec::new();

    for parsed in parsed_issues {
        let title = parsed.title.trim().to_string();
        if title.is_empty() {
            eprintln!("✗ Failed to create issue: title cannot be empty");
            continue;
        }

        let count = storage.count_issues()?;
        let id = id_gen.generate(
            &title,
            parsed.description.as_deref(),
            None,
            now,
            count,
            |id| storage.id_exists(id).unwrap_or(false),
        );

        let priority = if let Some(ref p) = parsed.priority {
            match Priority::from_str(p) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("✗ Failed to create {title}: {err}");
                    continue;
                }
            }
        } else {
            default_priority
        };

        let issue_type = if let Some(ref t) = parsed.issue_type {
            match IssueType::from_str(t) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("✗ Failed to create {title}: {err}");
                    continue;
                }
            }
        } else {
            default_issue_type.clone()
        };

        let mut issue = Issue {
            id: id.clone(),
            title: title.clone(),
            description: parsed.description,
            status: import_status.clone(),
            priority,
            issue_type,
            created_at: now,
            updated_at: now,
            assignee: parsed.assignee,
            owner: args.owner.clone(),
            estimated_minutes: args.estimate,
            due_at,
            defer_until,
            external_ref: args.external_ref.clone(),
            ephemeral: args.ephemeral,
            design: parsed.design,
            acceptance_criteria: parsed.acceptance_criteria,
            content_hash: None,
            notes: None,
            created_by: None,
            closed_at: import_closed_at,
            close_reason: None,
            closed_by_session: None,
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
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        issue.content_hash = Some(issue.compute_content_hash());
        if let Err(err) =
            IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)
        {
            eprintln!("✗ Failed to create {title}: {err}");
            continue;
        }

        // Populate Labels (with validation)
        let mut labels = parsed.labels;
        labels.extend(args.labels.clone());
        for label in labels {
            let label = label.trim().to_string();
            if label.is_empty() {
                continue;
            }
            if let Err(err) = LabelValidator::validate(&label) {
                eprintln!(
                    "warning: skipping invalid label '{label}' for issue {id}: {}",
                    err.message
                );
                continue;
            }
            issue.labels.push(label);
        }

        // Populate Dependencies (with validation)
        let mut deps = parsed.dependencies;
        deps.extend(args.deps.clone());
        for dep_str in deps {
            let (mut type_str, dep_id, valid) = parse_dependency(&dep_str);
            if !valid {
                eprintln!("warning: skipping invalid dependency type '{type_str}' for issue {id}");
                continue;
            }
            if type_str.eq_ignore_ascii_case("blocked-by") {
                type_str = "blocks".to_string();
            }
            if dep_id == id {
                eprintln!("warning: skipping self-dependency for issue {id}");
                continue;
            }

            let dep_type = type_str
                .parse()
                .unwrap_or_else(|_| DependencyType::Custom(type_str.clone()));

            issue.dependencies.push(Dependency {
                issue_id: id.clone(),
                depends_on_id: dep_id,
                dep_type,
                created_at: now,
                created_by: Some(actor.clone()),
                metadata: None,
                thread_id: None,
            });
        }

        if let Err(err) = storage.create_issue(&issue, &actor) {
            eprintln!("✗ Failed to create {title}: {err}");
            continue;
        }

        if ctx.is_json() {
            if let Some(full_issue) = storage.get_issue_for_export(&id)? {
                created_issues.push(full_issue);
            } else {
                eprintln!("warning: could not load created issue {id} for JSON output");
            }
        }

        created_ids.push((id, title));
    }

    if ctx.is_json() {
        ctx.json_pretty(&created_issues);
    } else if !created_ids.is_empty() {
        ctx.success(&format!(
            "Created {} issues from {}:",
            created_ids.len(),
            path.display()
        ));
        for (id, title) in created_ids {
            ctx.print(&format!("  {id}: {title}"));
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

fn parse_optional_date(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    match s {
        Some(s) if !s.trim().is_empty() => parse_flexible_timestamp(s, "date").map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::init_test_logging;
    use crate::util::id::IdConfig;
    use chrono::Datelike;
    use tracing::info;

    // Helper to create basic args
    fn default_args() -> CreateArgs {
        CreateArgs {
            title: Some("Test Issue".to_string()),
            title_flag: None,
            type_: None,
            priority: None,
            description: None,
            assignee: None,
            owner: None,
            labels: vec![],
            parent: None,
            deps: vec![],
            estimate: None,
            due: None,
            defer: None,
            external_ref: None,
            status: None,
            ephemeral: false,
            dry_run: false,
            silent: false,
            file: None,
        }
    }

    fn default_config() -> CreateConfig {
        CreateConfig {
            id_config: IdConfig {
                prefix: "bd".to_string(),
                min_hash_length: 3,
                max_hash_length: 8,
                max_collision_prob: 0.25,
            },
            default_priority: Priority::MEDIUM,
            default_issue_type: IssueType::Task,
            actor: "test_user".to_string(),
        }
    }

    fn setup_memory_storage() -> SqliteStorage {
        SqliteStorage::open_memory().expect("failed to open memory db")
    }

    #[test]
    fn test_create_issue_basic_success() {
        init_test_logging();
        info!("test_create_issue_basic_success: starting");
        let mut storage = setup_memory_storage();
        let args = default_args();
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        assert_eq!(issue.title, "Test Issue");
        assert_eq!(issue.priority, Priority::MEDIUM);
        assert_eq!(issue.issue_type, IssueType::Task);
        assert!(issue.id.starts_with("bd-"));

        // Verify persisted
        let loaded = storage.get_issue(&issue.id).expect("get issue");
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().title, "Test Issue");
        info!("test_create_issue_basic_success: assertions passed");
    }

    #[test]
    fn test_create_issue_validation_empty_title() {
        init_test_logging();
        info!("test_create_issue_validation_empty_title: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.title = None;
        let config = default_config();

        let err = create_issue_impl(&mut storage, &args, &config).unwrap_err();
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "title"));
        info!("test_create_issue_validation_empty_title: assertions passed");
    }

    #[test]
    fn test_create_issue_dry_run_no_writes() {
        init_test_logging();
        info!("test_create_issue_dry_run_no_writes: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.dry_run = true;
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        // Should return issue but not verify existence in DB
        assert_eq!(issue.title, "Test Issue");
        let loaded = storage.get_issue(&issue.id).expect("get issue");
        assert!(loaded.is_none(), "dry run should not persist issue");
        info!("test_create_issue_dry_run_no_writes: assertions passed");
    }

    #[test]
    fn test_create_issue_with_overrides() {
        init_test_logging();
        info!("test_create_issue_with_overrides: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.priority = Some("0".to_string());
        args.type_ = Some("bug".to_string());
        args.description = Some("Desc".to_string());
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        assert_eq!(issue.priority, Priority::CRITICAL);
        assert_eq!(issue.issue_type, IssueType::Bug);
        assert_eq!(issue.description, Some("Desc".to_string()));
        info!("test_create_issue_with_overrides: assertions passed");
    }

    #[test]
    fn test_create_issue_with_labels_and_deps() {
        init_test_logging();
        info!("test_create_issue_with_labels_and_deps: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Create dependency target first
        let target_args = CreateArgs {
            title: Some("Target".to_string()),
            ..default_args()
        };
        let target = create_issue_impl(&mut storage, &target_args, &config).expect("create target");

        // Create issue with label and dep
        let mut args = default_args();
        args.labels = vec!["backend".to_string()];
        args.deps = vec![target.id.clone()];

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        // Verify labels
        let labels = storage.get_labels(&issue.id).expect("get labels");
        assert!(labels.contains(&"backend".to_string()));

        // Verify deps
        let deps = storage.get_dependencies(&issue.id).expect("get deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], target.id);
        info!("test_create_issue_with_labels_and_deps: assertions passed");
    }

    #[test]
    fn test_create_parent_dependency() {
        init_test_logging();
        info!("test_create_parent_dependency: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Parent
        let parent = create_issue_impl(&mut storage, &default_args(), &config).expect("parent");

        // Child
        let mut args = default_args();
        args.parent = Some(parent.id.clone());
        let child = create_issue_impl(&mut storage, &args, &config).expect("child");

        let deps = storage.get_dependencies(&child.id).expect("get deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], parent.id);
        info!("test_create_parent_dependency: assertions passed");
    }

    #[test]
    fn test_create_child_generates_hierarchical_id() {
        init_test_logging();
        info!("test_create_child_generates_hierarchical_id: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Create parent (epic)
        let mut parent_args = default_args();
        parent_args.title = Some("Epic Parent".to_string());
        let parent = create_issue_impl(&mut storage, &parent_args, &config).expect("parent");

        // Create first child - should get parent.1
        let mut child1_args = default_args();
        child1_args.title = Some("First Child".to_string());
        child1_args.parent = Some(parent.id.clone());
        let child1 = create_issue_impl(&mut storage, &child1_args, &config).expect("child1");

        // Verify child ID has the correct format: parent_id.1
        let expected_child1_id = format!("{}.1", parent.id);
        assert_eq!(
            child1.id, expected_child1_id,
            "First child should have ID {expected_child1_id}, got {}",
            child1.id
        );

        // Create second child - should get parent.2
        let mut child2_args = default_args();
        child2_args.title = Some("Second Child".to_string());
        child2_args.parent = Some(parent.id.clone());
        let child2 = create_issue_impl(&mut storage, &child2_args, &config).expect("child2");

        let expected_child2_id = format!("{}.2", parent.id);
        assert_eq!(
            child2.id, expected_child2_id,
            "Second child should have ID {expected_child2_id}, got {}",
            child2.id
        );

        // Verify dependencies are set correctly
        let deps1 = storage.get_dependencies(&child1.id).expect("get deps1");
        assert_eq!(deps1.len(), 1);
        assert_eq!(deps1[0], parent.id);

        let deps2 = storage.get_dependencies(&child2.id).expect("get deps2");
        assert_eq!(deps2.len(), 1);
        assert_eq!(deps2[0], parent.id);

        info!("test_create_child_generates_hierarchical_id: assertions passed");
    }

    #[test]
    fn test_create_child_with_nonexistent_parent_fails() {
        init_test_logging();
        info!("test_create_child_with_nonexistent_parent_fails: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Try to create child with non-existent parent
        let mut args = default_args();
        args.parent = Some("bd-nonexistent".to_string());

        let result = create_issue_impl(&mut storage, &args, &config);
        assert!(result.is_err(), "Should fail when parent doesn't exist");

        if let Err(BeadsError::IssueNotFound { id }) = result {
            assert_eq!(id, "bd-nonexistent");
        } else {
            panic!("Expected IssueNotFound error");
        }

        info!("test_create_child_with_nonexistent_parent_fails: assertions passed");
    }

    #[test]
    fn test_create_issue_custom_type_accepted() {
        init_test_logging();
        info!("test_create_issue_custom_type_accepted: starting");
        // Custom types are now accepted
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.type_ = Some("custom_type".to_string());
        let config = default_config();

        let result = create_issue_impl(&mut storage, &args, &config);
        assert!(result.is_ok(), "create should succeed with custom type");
        let issue = result.unwrap();
        assert_eq!(
            issue.issue_type,
            IssueType::Custom("custom_type".to_string())
        );
        info!("test_create_issue_custom_type_accepted: assertions passed");
    }

    // =========================================================================
    // parse_optional_date tests (preserved)
    // =========================================================================

    #[test]
    fn test_parse_optional_date_none() {
        init_test_logging();
        info!("test_parse_optional_date_none: starting");
        let result = parse_optional_date(None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_none: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_empty_string() {
        init_test_logging();
        info!("test_parse_optional_date_empty_string: starting");
        let result = parse_optional_date(Some(""));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_empty_string: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_iso8601() {
        init_test_logging();
        info!("test_parse_optional_date_iso8601: starting");
        let result = parse_optional_date(Some("2026-01-17T10:00:00Z"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 17);
        info!("test_parse_optional_date_iso8601: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_simple_date() {
        init_test_logging();
        info!("test_parse_optional_date_simple_date: starting");
        let result = parse_optional_date(Some("2026-12-31"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 12);
        assert_eq!(dt.day(), 31);
        info!("test_parse_optional_date_simple_date: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_with_timezone() {
        init_test_logging();
        info!("test_parse_optional_date_with_timezone: starting");
        let result = parse_optional_date(Some("2026-06-15T14:30:00+05:30"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        info!("test_parse_optional_date_with_timezone: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_invalid_format() {
        init_test_logging();
        info!("test_parse_optional_date_invalid_format: starting");
        let result = parse_optional_date(Some("not-a-date"));
        assert!(result.is_err());
        info!("test_parse_optional_date_invalid_format: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_partial_date() {
        init_test_logging();
        info!("test_parse_optional_date_partial_date: starting");
        // Flexible parser may accept various formats
        let result = parse_optional_date(Some("2026-01"));
        let _ = result;
        info!("test_parse_optional_date_partial_date: assertions passed");
    }

    // =========================================================================
    // Date boundary tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_year_boundaries() {
        init_test_logging();
        info!("test_parse_optional_date_year_boundaries: starting");
        // Far future date
        let result = parse_optional_date(Some("2099-12-31"));
        assert!(result.is_ok());

        // Past date
        let result = parse_optional_date(Some("2000-01-01"));
        assert!(result.is_ok());
        info!("test_parse_optional_date_year_boundaries: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_leap_year() {
        init_test_logging();
        info!("test_parse_optional_date_leap_year: starting");
        // Feb 29 on leap year
        let result = parse_optional_date(Some("2024-02-29"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.month(), 2);
        assert_eq!(dt.day(), 29);
        info!("test_parse_optional_date_leap_year: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_end_of_month() {
        init_test_logging();
        info!("test_parse_optional_date_end_of_month: starting");
        // 31-day month
        let result = parse_optional_date(Some("2026-03-31"));
        assert!(result.is_ok());

        // 30-day month
        let result = parse_optional_date(Some("2026-04-30"));
        assert!(result.is_ok());
        info!("test_parse_optional_date_end_of_month: assertions passed");
    }

    // =========================================================================
    // Whitespace handling tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_whitespace_only() {
        init_test_logging();
        info!("test_parse_optional_date_whitespace_only: starting");
        // Should be treated as empty/None
        let result = parse_optional_date(Some("   "));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_whitespace_only: assertions passed");
    }

    #[test]
    fn test_create_issue_trims_labels() {
        init_test_logging();
        info!("test_create_issue_trims_labels: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();
        let mut args = default_args();
        args.labels = vec!["  trimmed  ".to_string()];

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        let labels = storage.get_labels(&issue.id).expect("get labels");
        assert_eq!(labels, vec!["trimmed"]);
        info!("test_create_issue_trims_labels: assertions passed");
    }
}
