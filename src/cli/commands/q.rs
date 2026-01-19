use crate::cli::QuickArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, IssueType, Priority, Status};
use crate::util::id::IdGenerator;
use crate::validation::LabelValidator;
use chrono::Utc;
use std::path::Path;
use std::str::FromStr;

fn split_labels(values: &[String]) -> Vec<String> {
    let mut labels = Vec::new();
    for value in values {
        for part in value.split(',') {
            let label = part.trim();
            if !label.is_empty() {
                labels.push(label.to_string());
            }
        }
    }
    labels
}

/// Execute the quick capture command.
///
/// # Errors
///
/// Returns an error if validation fails, the database cannot be opened, or creation fails.
pub fn execute(args: QuickArgs, cli: &config::CliOverrides) -> Result<()> {
    let title = args.title.join(" ").trim().to_string();
    if title.is_empty() {
        return Err(BeadsError::validation("title", "cannot be empty"));
    }

    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&layer);
    let default_priority = config::default_priority_from_layer(&layer)?;
    let default_issue_type = config::default_issue_type_from_layer(&layer)?;
    let storage = &mut storage_ctx.storage;

    let priority = if let Some(p) = args.priority {
        Priority::from_str(&p)?
    } else {
        default_priority
    };

    let issue_type = if let Some(t) = args.type_ {
        IssueType::from_str(&t)?
    } else {
        default_issue_type
    };

    let id_gen = IdGenerator::new(id_config);
    let now = Utc::now();
    let count = storage.count_issues()?;

    let id = id_gen.generate(&title, None, None, now, count, |candidate| {
        storage.id_exists(candidate).unwrap_or(false)
    });

    let mut issue = Issue {
        id,
        title,
        description: None,
        status: Status::Open,
        priority,
        issue_type,
        created_at: now,
        updated_at: now,
        content_hash: None,
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
    };

    // Resolve actor and set created_by
    let actor = config::resolve_actor(&layer);
    issue.created_by = Some(actor.clone());

    // Compute content hash
    issue.content_hash = Some(issue.compute_content_hash());

    storage.create_issue(&issue, &actor)?;

    let labels = split_labels(&args.labels);
    for label in labels {
        if let Err(err) = LabelValidator::validate(&label) {
            eprintln!("Warning: invalid label '{label}': {}", err.message);
            continue;
        }

        if let Err(err) = storage.add_label(&issue.id, &label, &actor) {
            eprintln!("Warning: failed to add label '{label}': {err}");
        }
    }

    println!("{}", issue.id);

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}
