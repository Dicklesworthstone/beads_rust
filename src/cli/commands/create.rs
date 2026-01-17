use crate::cli::CreateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, IssueType, Priority, Status};
use crate::util::id::IdGenerator;
use crate::util::markdown_import::parse_markdown_file;
use crate::util::time::parse_flexible_timestamp;
use crate::validation::{IssueValidator, LabelValidator};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::str::FromStr;

/// Execute the create command.
///
/// # Errors
///
/// Returns an error if validation fails, the database cannot be opened, or the issue cannot be created.
#[allow(clippy::too_many_lines)]
pub fn execute(args: CreateArgs, cli: &config::CliOverrides) -> Result<()> {
    if let Some(ref file_path) = args.file {
        return execute_import(file_path, &args, cli);
    }

    // 1. Resolve title
    let title = args
        .title
        .or(args.title_flag)
        .ok_or_else(|| BeadsError::validation("title", "cannot be empty"))?;

    if title.is_empty() {
        return Err(BeadsError::validation("title", "cannot be empty"));
    }

    // 2. Open storage (unless dry run without DB)
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;

    // We open storage even for dry-run to check ID collisions.
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&layer);
    let default_priority = config::default_priority_from_layer(&layer)?;
    let default_issue_type = config::default_issue_type_from_layer(&layer)?;
    let storage = &mut storage_ctx.storage;

    // 3. Generate ID
    let id_gen = IdGenerator::new(id_config);
    let now = Utc::now();
    let count = storage.count_issues()?;

    let id = id_gen.generate(
        &title,
        None, // description
        None, // creator
        now,
        count,
        |id| storage.id_exists(id).unwrap_or(false),
    );

    // 4. Parse fields
    let priority = if let Some(p) = args.priority {
        Priority::from_str(&p)?
    } else {
        default_priority
    };

    let issue_type = if let Some(t) = args.type_ {
        IssueType::from_str(&t)?
    } else {
        default_issue_type.clone()
    };

    let due_at = parse_optional_date(args.due.as_deref())?;
    let defer_until = parse_optional_date(args.defer.as_deref())?;

    // 5. Construct Issue
    let mut issue = Issue {
        id: id.clone(),
        title,
        description: args.description,
        status: Status::Open,
        priority,
        issue_type,
        created_at: now,
        updated_at: now,
        assignee: args.assignee,
        owner: args.owner,
        estimated_minutes: args.estimate,
        due_at,
        defer_until,
        external_ref: args.external_ref,
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

    // 5.5 Validate Issue
    IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

    // 6. Dry Run check
    if args.dry_run {
        if args.silent {
            println!("{}", issue.id);
        } else if cli.json.unwrap_or(false) {
            println!("{}", serde_json::to_string_pretty(&issue)?);
        } else {
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
        }
        return Ok(());
    }

    // 7. Create
    let actor = config::resolve_actor(&layer);
    storage.create_issue(&issue, &actor)?;

    // 8. Add auxiliary data
    // Labels
    for label in args.labels {
        let label = label.trim();
        if !label.is_empty() {
            LabelValidator::validate(label)
                .map_err(|e| BeadsError::validation("label", e.message))?;
            storage.add_label(&id, label, &actor)?;
        }
    }

    // Parent
    if let Some(parent_id) = args.parent {
        // Resolve parent ID if needed? usually assume exact or use resolve logic.
        // For simple create, we can just try to add dependency.
        // But better to verify it exists if we want robustness.
        // SqliteStorage::add_dependency checks foreign keys (issue_id) but depends_on_id is not FK enforced in schema for external refs.
        // However, standard deps usually require existence.
        // Let's rely on storage.add_dependency to handle it (or create logic).
        // Since we don't have resolver loaded here, we skip fuzzy resolution for simplicity or load it.
        // Let's assume exact ID for now to match current create logic simplicity,
        // OR reuse the ID resolver from config if we want to support prefixes.
        // Let's just use the string as provided, but validate not self.

        if parent_id == id {
            return Err(BeadsError::validation(
                "parent",
                "cannot be parent of itself",
            ));
        }
        storage.add_dependency(&id, &parent_id, "parent-child", &actor)?;
    }

    // Dependencies
    for dep_str in args.deps {
        let (type_str, dep_id) = if let Some((t, i)) = dep_str.split_once(':') {
            (t, i)
        } else {
            ("blocks", dep_str.as_str())
        };

        if dep_id == id {
            return Err(BeadsError::validation("deps", "cannot depend on itself"));
        }
        storage.add_dependency(&id, dep_id, type_str, &actor)?;
    }

    // 9. Output
    if args.silent {
        println!("{}", issue.id);
    } else if cli.json.unwrap_or(false) {
        // Re-fetch to get complete object with labels/deps?
        // Or just print what we created?
        // create_issue doesn't return full issue.
        // Let's just print the struct we made, but with labels/deps field populated manually for display
        // For now, print simple JSON of created object
        println!("{}", serde_json::to_string_pretty(&issue)?);
    } else {
        println!("Created {}: {}", issue.id, issue.title);
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

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
            // Use defaults or optional overrides from args (e.g. owner) if applicable?
            // Usually import uses data from file only, but args might set defaults?
            // bd create --file x.md --owner me -> applies to all?
            // For now, let's strictly use file content + defaults.
            owner: args.owner.clone(),
            estimated_minutes: args.estimate, // Apply global estimate if provided? Or None.
            // Let's apply global args as overrides/defaults where appropriate
            due_at: parse_optional_date(args.due.as_deref())?,
            defer_until: parse_optional_date(args.defer.as_deref())?,
            external_ref: args.external_ref.clone(),
            ephemeral: args.ephemeral,
            // File specific fields
            design: parsed.design,
            acceptance_criteria: parsed.acceptance_criteria,
            // ...
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

        // Compute content hash
        issue.content_hash = Some(issue.compute_content_hash());

        // Validate
        IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

        // Dry run
        if args.dry_run {
            println!("Dry run: would create issue {}", issue.id);
            continue;
        }

        // Create
        storage.create_issue(&issue, &actor)?;

        // Labels
        // Combine file labels and CLI labels
        let mut labels = parsed.labels;
        labels.extend(args.labels.clone());
        for label in labels {
            if !label.trim().is_empty() {
                LabelValidator::validate(&label)
                    .map_err(|e| BeadsError::validation("label", e.message))?;
                storage.add_label(&id, &label, &actor)?;
            }
        }

        // Dependencies
        // File deps format: "type:id" or "id"
        // CLI deps format: "type:id" or "id"
        let mut deps = parsed.dependencies;
        deps.extend(args.deps.clone());
        for dep_str in deps {
            let (type_str, dep_id) = if let Some((t, i)) = dep_str.split_once(':') {
                (t, i)
            } else {
                ("blocks", dep_str.as_str())
            };
            if dep_id == id {
                // Self dep ignored or error? Error is safer.
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
