//! Label command implementation.
//!
//! Provides label management: add, remove, list, list-all, and rename.

use crate::cli::{LabelAddArgs, LabelCommands, LabelListArgs, LabelRemoveArgs, LabelRenameArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use rich_rust::prelude::*;
use serde::Serialize;
use std::path::Path;
use tracing::{debug, info};

/// Execute the label command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(
    command: &LabelCommands,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let actor = config::resolve_actor(&config_layer);
    let storage = &mut storage_ctx.storage;

    match command {
        LabelCommands::Add(args) => {
            label_add(args, storage, &resolver, &all_ids, &actor, json, ctx)
        }
        LabelCommands::Remove(args) => {
            label_remove(args, storage, &resolver, &all_ids, &actor, json, ctx)
        }
        LabelCommands::List(args) => label_list(args, storage, &resolver, &all_ids, json, ctx),
        LabelCommands::ListAll => label_list_all(storage, json, ctx),
        LabelCommands::Rename(args) => label_rename(args, storage, &actor, json, ctx),
    }?;

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
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
    _json: bool,
    ctx: &OutputContext,
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

    if ctx.is_json() {
        ctx.json_pretty(&results);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_label_action_results_rich(&results, "add", ctx);
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
    _json: bool,
    ctx: &OutputContext,
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

    if ctx.is_json() {
        ctx.json_pretty(&results);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_label_action_results_rich(&results, "remove", ctx);
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
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if let Some(input) = &args.issue {
        // List labels for a specific issue
        let issue_id = resolve_issue_id(storage, resolver, all_ids, input)?;
        let labels = storage.get_labels(&issue_id)?;

        if ctx.is_json() {
            ctx.json_pretty(&labels);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_labels_for_issue_rich(&issue_id, &labels, ctx);
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
        let labels_with_counts = storage.get_unique_labels_with_counts()?;
        let unique_labels: Vec<String> = labels_with_counts.into_iter().map(|(l, _)| l).collect();

        if ctx.is_json() {
            ctx.json_pretty(&unique_labels);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_unique_labels_rich(&unique_labels, ctx);
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

fn label_list_all(storage: &SqliteStorage, _json: bool, ctx: &OutputContext) -> Result<()> {
    let labels_with_counts = storage.get_unique_labels_with_counts()?;

    let label_counts: Vec<LabelCount> = labels_with_counts
        .into_iter()
        .map(|(label, count)| LabelCount {
            label,
            count: usize::try_from(count).unwrap_or(0),
        })
        .collect();

    if ctx.is_json() {
        ctx.json_pretty(&label_counts);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_label_counts_rich(&label_counts, ctx);
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
    _json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    validate_label(&args.new_name)?;

    info!(
        old = %args.old_name,
        new = %args.new_name,
        "Renaming label"
    );

    let count = storage.rename_label(&args.old_name, &args.new_name, actor)?;

    if count == 0 {
        if ctx.is_json() {
            let result = RenameResult {
                old_name: args.old_name.clone(),
                new_name: args.new_name.clone(),
                affected_issues: 0,
            };
            ctx.json_pretty(&result);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_rename_not_found_rich(&args.old_name, ctx);
        } else {
            println!("Label '{}' not found on any issues.", args.old_name);
        }
        return Ok(());
    }

    if ctx.is_json() {
        let result = RenameResult {
            old_name: args.old_name.clone(),
            new_name: args.new_name.clone(),
            affected_issues: count,
        };
        ctx.json_pretty(&result);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_rename_result_rich(&args.old_name, &args.new_name, count, ctx);
    } else {
        println!(
            "\u{2713} Renamed label '{}' to '{}' on {} issue{}",
            args.old_name,
            args.new_name,
            count,
            if count == 1 { "" } else { "s" }
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

// ============================================================================
// Rich Output Rendering Functions
// ============================================================================

/// Get a consistent color for a label based on its name hash.
fn label_color(label: &str) -> Color {
    // Color palette for labels - varied but readable colors
    const LABEL_PALETTE: &[&str] = &[
        "cyan",
        "green",
        "yellow",
        "magenta",
        "blue",
        "bright_cyan",
        "bright_green",
        "bright_yellow",
        "bright_magenta",
        "bright_blue",
    ];

    let hash = label.bytes().fold(0u8, u8::wrapping_add);
    let color_name = LABEL_PALETTE[hash as usize % LABEL_PALETTE.len()];
    Color::parse(color_name).unwrap_or_default()
}

/// Render label add/remove action results in rich mode.
fn render_label_action_results_rich(
    results: &[LabelActionResult],
    action: &str,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();

    for result in results {
        let mut text = Text::new("");

        let (icon, verb, style) = if action == "add" {
            if result.status == "added" {
                ("\u{2713}", "Added", theme.success.clone())
            } else {
                ("\u{2022}", "Exists", theme.dimmed.clone())
            }
        } else {
            // remove
            if result.status == "removed" {
                ("\u{2713}", "Removed", theme.success.clone())
            } else {
                ("\u{2022}", "Not found", theme.dimmed.clone())
            }
        };

        text.append_styled(&format!("{icon} {verb} label "), style);
        text.append_styled(
            &result.label,
            Style::new().color(label_color(&result.label)),
        );
        text.append(if action == "add" { " on " } else { " from " });
        text.append_styled(&result.issue_id, theme.issue_id.clone());

        console.print_renderable(&text);
    }
}

/// Render labels for a specific issue in rich mode.
fn render_labels_for_issue_rich(issue_id: &str, labels: &[String], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if labels.is_empty() {
        let mut text = Text::new("");
        text.append_styled("No labels for ", theme.dimmed.clone());
        text.append_styled(issue_id, theme.issue_id.clone());
        console.print_renderable(&text);
        return;
    }

    let mut text = Text::new("");
    text.append("Labels for ");
    text.append_styled(issue_id, theme.issue_id.clone());
    text.append(":");
    console.print_renderable(&text);

    // Display labels on a single line with spacing
    let mut label_line = Text::new("  ");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            label_line.append("  ");
        }
        label_line.append_styled(label, Style::new().color(label_color(label)));
    }
    console.print_renderable(&label_line);
}

/// Render unique labels list in rich mode.
fn render_unique_labels_rich(labels: &[String], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if labels.is_empty() {
        let text = Text::styled("No labels in project.", theme.dimmed.clone());
        console.print_renderable(&text);
        return;
    }

    let mut header = Text::new("");
    header.append_styled("Labels ", Style::new().bold());
    header.append_styled(&format!("({} total)", labels.len()), theme.dimmed.clone());
    console.print_renderable(&header);

    // Display labels in a compact format
    let mut label_line = Text::new("  ");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            label_line.append("  ");
        }
        label_line.append_styled(label, Style::new().color(label_color(label)));
    }
    console.print_renderable(&label_line);
}

/// Render label counts (list-all) in rich mode with Panel.
fn render_label_counts_rich(label_counts: &[LabelCount], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    if label_counts.is_empty() {
        let text = Text::styled("No labels in project.", theme.dimmed.clone());
        console.print_renderable(&text);
        return;
    }

    let mut content = Text::new("");

    // Calculate total issues with labels
    let total_issues: usize = label_counts.iter().map(|lc| lc.count).sum();

    for (i, lc) in label_counts.iter().enumerate() {
        if i > 0 {
            content.append("\n");
        }
        content.append_styled(
            &format!("{:<20}", lc.label),
            Style::new().color(label_color(&lc.label)),
        );
        content.append_styled(
            &format!(
                "{:>4} issue{}",
                lc.count,
                if lc.count == 1 { "" } else { "s" }
            ),
            theme.dimmed.clone(),
        );
    }

    content.append("\n\n");
    content.append_styled(
        &format!(
            "Total: {} label{} across {} issue assignment{}",
            label_counts.len(),
            if label_counts.len() == 1 { "" } else { "s" },
            total_issues,
            if total_issues == 1 { "" } else { "s" }
        ),
        theme.dimmed.clone(),
    );

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::new("Project Labels"))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render rename not found message in rich mode.
fn render_rename_not_found_rich(old_name: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{26a0} ", theme.warning.clone());
    text.append("Label ");
    text.append_styled(old_name, Style::new().color(label_color(old_name)));
    text.append_styled(" not found on any issues.", theme.dimmed.clone());

    console.print_renderable(&text);
}

/// Render rename result in rich mode.
fn render_rename_result_rich(old_name: &str, new_name: &str, count: usize, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("Renamed ");
    text.append_styled(old_name, Style::new().color(label_color(old_name)).dim());
    text.append(" \u{2192} ");
    text.append_styled(new_name, Style::new().color(label_color(new_name)).bold());
    text.append_styled(
        &format!(" on {} issue{}", count, if count == 1 { "" } else { "s" }),
        theme.dimmed.clone(),
    );

    console.print_renderable(&text);
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
