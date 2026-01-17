//! Label command implementation.
//!
//! Provides label management: add, remove, list, list-all, and rename.

use crate::cli::{LabelAddArgs, LabelCommands, LabelListArgs, LabelRemoveArgs, LabelRenameArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info};

/// Execute the label command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(command: &LabelCommands, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;
    let actor = config::resolve_actor(&config_layer);

    match command {
        LabelCommands::Add(args) => {
            label_add(args, &mut storage, &resolver, &all_ids, &actor, json)
        }
        LabelCommands::Remove(args) => {
            label_remove(args, &mut storage, &resolver, &all_ids, &actor, json)
        }
        LabelCommands::List(args) => label_list(args, &storage, &resolver, &all_ids, json),
        LabelCommands::ListAll => label_list_all(&storage, json),
        LabelCommands::Rename(args) => label_rename(args, &mut storage, &actor, json),
    }
}

/// JSON output for label add/remove operations.
#[derive(Serialize)]
struct LabelActionResult {
    status: String,
    issue_id: String,
    label: String,
}

/// JSON output for list-all.
#[derive(Serialize)]
struct LabelCount {
    label: String,
    count: usize,
}

/// JSON output for rename.
#[derive(Serialize)]
struct RenameResult {
    old_name: String,
    new_name: String,
    affected_issues: usize,
}

/// Validate a label name.
///
/// Labels must be alphanumeric with dashes and underscores allowed.
fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() {
        return Err(BeadsError::validation("label", "label cannot be empty"));
    }

    // Validate characters: alphanumeric, dash, underscore, colon (for namespacing)
    for c in label.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != ':' {
            return Err(BeadsError::validation(
                "label",
                format!(
                    "Invalid label '{label}': only alphanumeric, dash, underscore, and colon allowed"
                ),
            ));
        }
    }

    Ok(())
}

/// Parse issues and label from positional args.
///
/// The last argument is the label, all preceding arguments are issue IDs.
fn parse_issues_and_label(
    issues: &[String],
    label_flag: Option<&String>,
) -> Result<(Vec<String>, String)> {
    // If label is provided via flag, all positional args are issues
    if let Some(label) = label_flag {
        if issues.is_empty() {
            return Err(BeadsError::validation(
                "issues",
                "at least one issue ID required",
            ));
        }
        return Ok((issues.to_vec(), label.clone()));
    }

    // Otherwise, last positional arg is the label
    if issues.len() < 2 {
        return Err(BeadsError::validation(
            "arguments",
            "usage: label add <issue...> <label> or label add <issue...> -l <label>",
        ));
    }

    let (issue_ids, label_args) = issues.split_at(issues.len() - 1);
    let label = label_args[0].clone();

    Ok((issue_ids.to_vec(), label))
}

fn label_add(
    args: &LabelAddArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
    json: bool,
) -> Result<()> {
    let (issue_inputs, label) = parse_issues_and_label(&args.issues, args.label.as_ref())?;

    validate_label(&label)?;

    let mut results = Vec::new();

    for input in &issue_inputs {
        let issue_id = resolve_issue_id(storage, resolver, all_ids, input)?;

        info!(issue_id = %issue_id, label = %label, "Adding label");

        let added = storage.add_label(&issue_id, &label, actor)?;

        debug!(already_exists = !added, "Label status check");

        if added {
            info!(issue_id = %issue_id, label = %label, "Label added");
        }

        results.push(LabelActionResult {
            status: if added { "added" } else { "exists" }.to_string(),
            issue_id: issue_id.clone(),
            label: label.clone(),
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in &results {
            if result.status == "added" {
                println!(
                    "\u{2713} Added label {} to {}",
                    result.label, result.issue_id
                );
            } else {
                println!(
                    "\u{2713} Label {} already exists on {}",
                    result.label, result.issue_id
                );
            }
        }
    }

    Ok(())
}

fn label_remove(
    args: &LabelRemoveArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: &str,
    json: bool,
) -> Result<()> {
    let (issue_inputs, label) = parse_issues_and_label(&args.issues, args.label.as_ref())?;

    let mut results = Vec::new();

    for input in &issue_inputs {
        let issue_id = resolve_issue_id(storage, resolver, all_ids, input)?;

        info!(issue_id = %issue_id, label = %label, "Removing label");

        let removed = storage.remove_label(&issue_id, &label, actor)?;

        results.push(LabelActionResult {
            status: if removed { "removed" } else { "not_found" }.to_string(),
            issue_id: issue_id.clone(),
            label: label.clone(),
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in &results {
            if result.status == "removed" {
                println!(
                    "\u{2713} Removed label {} from {}",
                    result.label, result.issue_id
                );
            } else {
                println!(
                    "\u{2713} Label {} not found on {} (no-op)",
                    result.label, result.issue_id
                );
            }
        }
    }

    Ok(())
}

fn label_list(
    args: &LabelListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    json: bool,
) -> Result<()> {
    if let Some(input) = &args.issue {
        // List labels for a specific issue
        let issue_id = resolve_issue_id(storage, resolver, all_ids, input)?;
        let labels = storage.get_labels(&issue_id)?;

        if json {
            println!("{}", serde_json::to_string_pretty(&labels)?);
        } else if labels.is_empty() {
            println!("No labels for {issue_id}.");
        } else {
            println!("Labels for {issue_id}:");
            for label in &labels {
                println!("  {label}");
            }
        }
    } else {
        // List all unique labels (without counts - use list-all for counts)
        let all_labels = storage.get_all_labels()?;
        let mut unique_labels: Vec<String> = all_labels.values().flatten().cloned().collect();
        unique_labels.sort();
        unique_labels.dedup();

        if json {
            println!("{}", serde_json::to_string_pretty(&unique_labels)?);
        } else if unique_labels.is_empty() {
            println!("No labels in project.");
        } else {
            println!("Labels ({} total):", unique_labels.len());
            for label in &unique_labels {
                println!("  {label}");
            }
        }
    }

    Ok(())
}

fn label_list_all(storage: &SqliteStorage, json: bool) -> Result<()> {
    let all_labels = storage.get_all_labels()?;

    // Count occurrences of each label
    let mut counts: HashMap<String, usize> = HashMap::new();
    for labels in all_labels.values() {
        for label in labels {
            *counts.entry(label.clone()).or_insert(0) += 1;
        }
    }

    // Sort by label name
    let mut label_counts: Vec<LabelCount> = counts
        .into_iter()
        .map(|(label, count)| LabelCount { label, count })
        .collect();
    label_counts.sort_by(|a, b| a.label.cmp(&b.label));

    if json {
        println!("{}", serde_json::to_string_pretty(&label_counts)?);
    } else if label_counts.is_empty() {
        println!("No labels in project.");
    } else {
        println!("Labels ({} total):", label_counts.len());
        for lc in &label_counts {
            println!(
                "  {} ({} issue{})",
                lc.label,
                lc.count,
                if lc.count == 1 { "" } else { "s" }
            );
        }
    }

    Ok(())
}

fn label_rename(
    args: &LabelRenameArgs,
    storage: &mut SqliteStorage,
    actor: &str,
    json: bool,
) -> Result<()> {
    validate_label(&args.new_name)?;

    let all_labels = storage.get_all_labels()?;

    // Find all issues with the old label
    let affected_issues: Vec<String> = all_labels
        .iter()
        .filter_map(|(issue_id, labels)| {
            if labels.contains(&args.old_name) {
                Some(issue_id.clone())
            } else {
                None
            }
        })
        .collect();

    if affected_issues.is_empty() {
        if json {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Label '{}' not found on any issues.", args.old_name);
        }
        return Ok(());
    }

    info!(
        old = %args.old_name,
        new = %args.new_name,
        count = affected_issues.len(),
        "Renaming label"
    );

    // Rename: remove old, add new for each affected issue
    for issue_id in &affected_issues {
        storage.remove_label(issue_id, &args.old_name, actor)?;
        storage.add_label(issue_id, &args.new_name, actor)?;
    }

    if json {
        let result = RenameResult {
            old_name: args.old_name.clone(),
            new_name: args.new_name.clone(),
            affected_issues: affected_issues.len(),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "\u{2713} Renamed label '{}' to '{}' on {} issue{}",
            args.old_name,
            args.new_name,
            affected_issues.len(),
            if affected_issues.len() == 1 { "" } else { "s" }
        );
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

    #[test]
    fn test_validate_label_valid() {
        assert!(validate_label("bug").is_ok());
        assert!(validate_label("high-priority").is_ok());
        assert!(validate_label("needs_review").is_ok());
        assert!(validate_label("v1_0").is_ok());
        assert!(validate_label("Bug123").is_ok());
        assert!(validate_label("team:backend").is_ok());
    }

    #[test]
    fn test_validate_label_invalid() {
        assert!(validate_label("").is_err());
        assert!(validate_label("has space").is_err());
        assert!(validate_label("special@char").is_err());
        assert!(validate_label("dot.not.allowed").is_err());
    }

    #[test]
    fn test_validate_label_namespaced_allows_provides() {
        assert!(validate_label("provides:auth").is_ok());
        assert!(validate_label("provides:").is_ok());
    }

    #[test]
    fn test_parse_issues_and_label_with_flag() {
        let issues = vec!["bd-abc".to_string(), "bd-def".to_string()];
        let label = Some("urgent".to_string());

        let (parsed_issues, parsed_label) =
            parse_issues_and_label(&issues, label.as_ref()).unwrap();
        assert_eq!(parsed_issues, vec!["bd-abc", "bd-def"]);
        assert_eq!(parsed_label, "urgent");
    }

    #[test]
    fn test_parse_issues_and_label_positional() {
        let issues = vec![
            "bd-abc".to_string(),
            "bd-def".to_string(),
            "urgent".to_string(),
        ];
        let label: Option<&String> = None;

        let (parsed_issues, parsed_label) = parse_issues_and_label(&issues, label).unwrap();
        assert_eq!(parsed_issues, vec!["bd-abc", "bd-def"]);
        assert_eq!(parsed_label, "urgent");
    }

    #[test]
    fn test_parse_issues_and_label_single_issue() {
        let issues = vec!["bd-abc".to_string(), "urgent".to_string()];
        let label: Option<&String> = None;

        let (parsed_issues, parsed_label) = parse_issues_and_label(&issues, label).unwrap();
        assert_eq!(parsed_issues, vec!["bd-abc"]);
        assert_eq!(parsed_label, "urgent");
    }

    #[test]
    fn test_parse_issues_and_label_missing_label() {
        let issues = vec!["bd-abc".to_string()];
        let label: Option<&String> = None;

        let result = parse_issues_and_label(&issues, label);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_issues_and_label_no_issues_with_flag() {
        let issues: Vec<String> = vec![];
        let label = Some("urgent".to_string());

        let result = parse_issues_and_label(&issues, label.as_ref());
        assert!(result.is_err());
    }
}
