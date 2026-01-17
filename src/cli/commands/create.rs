use crate::cli::CreateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use crate::storage::SqliteStorage;
use crate::util::id::IdGenerator;
use crate::util::markdown_import::parse_markdown_file;
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
pub fn execute(args: &CreateArgs, cli: &config::CliOverrides) -> Result<()> {
    if let Some(ref file_path) = args.file {
        return execute_import(file_path, args, cli);
    }

    // 1. Open storage (unless dry run without DB)
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;

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
    } else if cli.json.unwrap_or(false) {
        if args.dry_run {
            println!("{}", serde_json::to_string_pretty(&issue)?);
        } else {
            let full_issue = storage_ctx
                .storage
                .get_issue_for_export(&issue.id)?
                .ok_or_else(|| BeadsError::IssueNotFound {
                    id: issue.id.clone(),
                })?;
            println!("{}", serde_json::to_string_pretty(&full_issue)?);
        }
    } else if args.dry_run {
        println!("Dry run: would create issue {}", issue.id);
        println!("Title: {}", issue.title);
        println!("Type: {}", issue.issue_type);
        println!("Priority: {}", issue.priority);
        if !args.labels.is_empty() {
            println!("Labels: {}", args.labels.join(", "));
        }
        if let Some(parent) = &args.parent {
            println!("Parent: {parent}");
        }
        if !args.deps.is_empty() {
            println!("Dependencies: {}", args.deps.join(", "));
        }
    } else {
        println!("Created {}: {}", issue.id, issue.title);
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
    let id_gen = IdGenerator::new(config.id_config.clone());
    let now = Utc::now();
    let count = storage.count_issues()?;

    let id = id_gen.generate(
        title,
        None, // description
        None, // creator
        now,
        count,
        |id| storage.id_exists(id).unwrap_or(false),
    );

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

    // 4. Construct Issue
    let mut issue = Issue {
        id: id.clone(),
        title: title.clone(),
        description: args.description.clone(),
        status: Status::Open,
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
        created_by: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
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

    // 6. Dry Run check - return early
    if args.dry_run {
        return Ok(issue);
    }

    // 7. Create
    storage.create_issue(&issue, &config.actor)?;

    // 8. Add auxiliary data
    add_relations(storage, &id, args, &config.actor, &mut issue, now)?;

    Ok(issue)
}

fn validate_relations(args: &CreateArgs, id: &str) -> Result<()> {
    // Validate Labels
    for label in &args.labels {
        if !label.trim().is_empty() {
            LabelValidator::validate(label)
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

fn add_relations(
    storage: &mut SqliteStorage,
    id: &str,
    args: &CreateArgs,
    actor: &str,
    issue: &mut Issue,
    now: DateTime<Utc>,
) -> Result<()> {
    // Labels
    for label in &args.labels {
        let label = label.trim();
        if !label.is_empty() {
            // Validation already done in validate_relations
            storage.add_label(id, label, actor)?;
            issue.labels.push(label.to_string());
        }
    }

    // Parent
    if let Some(parent_id) = &args.parent {
        // Validation already done
        storage.add_dependency(id, parent_id, "parent-child", actor)?;

        issue.dependencies.push(Dependency {
            issue_id: id.to_string(),
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

        // Validation already done
        storage.add_dependency(id, dep_id, type_str, actor)?;

        let dep_type = type_str
            .parse()
            .unwrap_or_else(|_| DependencyType::Custom(type_str.to_string()));
        issue.dependencies.push(Dependency {
            issue_id: id.to_string(),
            depends_on_id: dep_id.to_string(),
            dep_type,
            created_at: now,
            created_by: Some(actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_import(path: &Path, args: &CreateArgs, cli: &config::CliOverrides) -> Result<()> {
    let parsed_issues = parse_markdown_file(path)?;
    if parsed_issues.is_empty() {
        return Ok(());
    }

    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;

    let id_config = config::id_config_from_layer(&layer);
    let default_priority = config::default_priority_from_layer(&layer)?;
    let default_issue_type = config::default_issue_type_from_layer(&layer)?;
    let actor = config::resolve_actor(&layer);
    let now = Utc::now();

    let storage = &mut storage_ctx.storage;
    let id_gen = IdGenerator::new(id_config);

    // Track created IDs for output
    let mut created_ids = Vec::new();

    for parsed in parsed_issues {
        let count = storage.count_issues()?;
        let id = id_gen.generate(
            &parsed.title,
            parsed.description.as_deref(),
            None,
            now,
            count,
            |id| storage.id_exists(id).unwrap_or(false),
        );

        let priority = if let Some(ref p) = parsed.priority {
            Priority::from_str(p)?
        } else {
            default_priority
        };

        let issue_type = if let Some(ref t) = parsed.issue_type {
            IssueType::from_str(t)?
        } else {
            default_issue_type.clone()
        };

        let mut issue = Issue {
            id: id.clone(),
            title: parsed.title,
            description: parsed.description,
            status: Status::Open,
            priority,
            issue_type,
            created_at: now,
            updated_at: now,
            assignee: parsed.assignee,
            owner: args.owner.clone(),
            estimated_minutes: args.estimate,
            due_at: parse_optional_date(args.due.as_deref())?,
            defer_until: parse_optional_date(args.defer.as_deref())?,
            external_ref: args.external_ref.clone(),
            ephemeral: args.ephemeral,
            design: parsed.design,
            acceptance_criteria: parsed.acceptance_criteria,
            content_hash: None,
            notes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
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
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        issue.content_hash = Some(issue.compute_content_hash());
        IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

        if args.dry_run {
            println!("Dry run: would create issue {}", issue.id);
            continue;
        }

        storage.create_issue(&issue, &actor)?;

        let mut labels = parsed.labels;
        labels.extend(args.labels.clone());
        for label in labels {
            if !label.trim().is_empty() {
                LabelValidator::validate(&label)
                    .map_err(|e| BeadsError::validation("label", e.message))?;
                storage.add_label(&id, &label, &actor)?;
            }
        }

        let mut deps = parsed.dependencies;
        deps.extend(args.deps.clone());
        for dep_str in deps {
            let (type_str, dep_id) = if let Some((t, i)) = dep_str.split_once(':') {
                (t, i)
            } else {
                ("blocks", dep_str.as_str())
            };
            if dep_id == id {
                return Err(BeadsError::validation(
                    "deps",
                    format!("issue {id} cannot depend on itself"),
                ));
            }
            storage.add_dependency(&id, dep_id, type_str, &actor)?;
        }

        created_ids.push(id);
    }

    if !created_ids.is_empty() && !args.dry_run {
        println!("Created {} issues:", created_ids.len());
        for id in created_ids {
            println!("  {id}");
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

fn parse_optional_date(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    match s {
        Some(s) if !s.is_empty() => parse_flexible_timestamp(s, "date").map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::id::IdConfig;
    use chrono::Datelike;

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
    }

    #[test]
    fn test_create_issue_validation_empty_title() {
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.title = None;
        let config = default_config();

        let err = create_issue_impl(&mut storage, &args, &config).unwrap_err();
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "title"));
    }

    #[test]
    fn test_create_issue_dry_run_no_writes() {
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.dry_run = true;
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config).expect("create failed");

        // Should return issue but not verify existence in DB
        assert_eq!(issue.title, "Test Issue");
        let loaded = storage.get_issue(&issue.id).expect("get issue");
        assert!(loaded.is_none(), "dry run should not persist issue");
    }

    #[test]
    fn test_create_issue_with_overrides() {
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
    }

    #[test]
    fn test_create_issue_with_labels_and_deps() {
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
    }

    #[test]
    fn test_create_parent_dependency() {
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
    }

    #[test]
    fn test_create_issue_custom_type() {
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.type_ = Some("invalid_type".to_string());
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config)
            .expect("create should succeed with custom type");
        assert_eq!(
            issue.issue_type,
            IssueType::Custom("invalid_type".to_string())
        );
    }

    // =========================================================================
    // parse_optional_date tests (preserved)
    // =========================================================================

    #[test]
    fn test_parse_optional_date_none() {
        let result = parse_optional_date(None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_parse_optional_date_empty_string() {
        let result = parse_optional_date(Some(""));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_parse_optional_date_iso8601() {
        let result = parse_optional_date(Some("2026-01-17T10:00:00Z"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 17);
    }

    #[test]
    fn test_parse_optional_date_simple_date() {
        let result = parse_optional_date(Some("2026-12-31"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 12);
        assert_eq!(dt.day(), 31);
    }

    #[test]
    fn test_parse_optional_date_with_timezone() {
        let result = parse_optional_date(Some("2026-06-15T14:30:00+05:30"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
    }

    #[test]
    fn test_parse_optional_date_invalid_format() {
        let result = parse_optional_date(Some("not-a-date"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_optional_date_partial_date() {
        // Flexible parser may accept various formats
        let result = parse_optional_date(Some("2026-01"));
        let _ = result;
    }

    // =========================================================================
    // Date boundary tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_year_boundaries() {
        // Far future date
        let result = parse_optional_date(Some("2099-12-31"));
        assert!(result.is_ok());

        // Past date
        let result = parse_optional_date(Some("2000-01-01"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_optional_date_leap_year() {
        // Feb 29 on leap year
        let result = parse_optional_date(Some("2024-02-29"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.month(), 2);
        assert_eq!(dt.day(), 29);
    }

    #[test]
    fn test_parse_optional_date_end_of_month() {
        // 31-day month
        let result = parse_optional_date(Some("2026-03-31"));
        assert!(result.is_ok());

        // 30-day month
        let result = parse_optional_date(Some("2026-04-30"));
        assert!(result.is_ok());
    }

    // =========================================================================
    // Whitespace handling tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_whitespace_only() {
        // Should be treated as non-empty by the string check, but may fail parsing
        let result = parse_optional_date(Some("   "));
        // Behavior depends on implementation - just ensure no panic
        let _ = result;
    }
}
