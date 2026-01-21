//! Lint command implementation.
//!
//! Checks issues for missing recommended template sections based on issue type.

use crate::cli::LintArgs;
use crate::config;
use crate::error::Result;
use crate::model::{Issue, IssueType, Status};
use crate::output::OutputContext;
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::{IdResolver, ResolverConfig};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize)]
struct LintResult {
    id: String,
    title: String,
    #[serde(rename = "type")]
    issue_type: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing: Vec<String>,
    warnings: usize,
}

#[derive(Debug, Serialize)]
struct LintOutput {
    total: usize,
    issues: usize,
    results: Vec<LintResult>,
}

#[derive(Debug)]
struct LintSummary {
    checked: usize,
    warnings: usize,
    results: Vec<LintResult>,
}

impl LintSummary {
    const fn exit_code(&self, json: bool) -> i32 {
        if json || self.warnings == 0 { 0 } else { 1 }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequiredSection {
    heading: &'static str,
    #[allow(dead_code)] // Kept for future use in suggestions
    hint: &'static str,
}

const BUG_SECTIONS: [RequiredSection; 2] = [
    RequiredSection {
        heading: "## Steps to Reproduce",
        hint: "Describe how to reproduce the bug",
    },
    RequiredSection {
        heading: "## Acceptance Criteria",
        hint: "Define criteria to verify the fix",
    },
];

const TASK_SECTIONS: [RequiredSection; 1] = [RequiredSection {
    heading: "## Acceptance Criteria",
    hint: "Define criteria to verify completion",
}];

const EPIC_SECTIONS: [RequiredSection; 1] = [RequiredSection {
    heading: "## Success Criteria",
    hint: "Define high-level success criteria",
}];

/// Execute the lint command.
///
/// # Errors
///
/// Returns an error if database access fails or filters are invalid.
pub fn execute(
    args: &LintArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    let issues = if args.ids.is_empty() {
        let filters = build_filters(args)?;
        storage.list_issues(&filters)?
    } else {
        resolve_issues(storage, &beads_dir, args, cli)?
    };

    let summary = lint_issues(&issues);

    if ctx.is_json() {
        let output = LintOutput {
            total: summary.warnings,
            issues: summary.results.len(),
            results: summary.results,
        };
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_quiet() {
        if summary.results.is_empty() {
            return Ok(());
        }
        std::process::exit(summary.exit_code(false));
    }

    if ctx.is_rich() {
        render_lint_rich(&summary, ctx);
    } else {
        if summary.results.is_empty() {
            println!(
                "✓ No template warnings found ({} issues checked)",
                summary.checked
            );
            return Ok(());
        }

        println!(
            "Template warnings ({} issues, {} warnings):\n",
            summary.results.len(),
            summary.warnings
        );
        for result in &summary.results {
            println!("{} [{}]: {}", result.id, result.issue_type, result.title);
            for missing in &result.missing {
                println!("  ⚠ Missing: {missing}");
            }
            println!();
        }
    }

    std::process::exit(summary.exit_code(false));
}

fn render_lint_rich(summary: &LintSummary, ctx: &OutputContext) {
    let theme = ctx.theme();
    let mut content = Text::new("");

    content.append_styled("Template Lint\n", theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Checked: ", theme.dimmed.clone());
    content.append_styled(&summary.checked.to_string(), theme.emphasis.clone());
    content.append_styled("    Warnings: ", theme.dimmed.clone());
    if summary.warnings == 0 {
        content.append_styled("0", theme.success.clone());
    } else {
        content.append_styled(&summary.warnings.to_string(), theme.warning.clone());
    }
    content.append("\n\n");

    if summary.results.is_empty() {
        content.append_styled(
            &format!(
                "✓ No template warnings found ({} issues checked)",
                summary.checked
            ),
            theme.success.clone(),
        );
    } else {
        let mut by_type: BTreeMap<&str, Vec<&LintResult>> = BTreeMap::new();
        for result in &summary.results {
            by_type
                .entry(result.issue_type.as_str())
                .or_default()
                .push(result);
        }

        for (issue_type, results) in by_type {
            content.append_styled(
                &format!("{issue_type} ({})\n", results.len()),
                theme.section.clone(),
            );
            for result in results {
                content.append_styled("- ", theme.warning.clone());
                content.append_styled(&result.id, theme.issue_id.clone());
                content.append(" ");
                content.append_styled(
                    &format!("[{}] ", result.issue_type),
                    issue_type_style(theme, &result.issue_type),
                );
                content.append_styled(&result.title, theme.issue_title.clone());
                content.append("\n");

                for missing in &result.missing {
                    content.append_styled("    missing: ", theme.dimmed.clone());
                    content.append_styled(missing, theme.warning.clone());
                    content.append("\n");
                }
            }
            content.append("\n");
        }

        content.append_styled(
            "Tip: Add the missing sections to issue descriptions to clear warnings.\n",
            theme.dimmed.clone(),
        );
    }

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::styled("Lint Results", theme.panel_title.clone()))
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone());

    ctx.render(&panel);
}

fn issue_type_style(theme: &crate::output::Theme, issue_type: &str) -> Style {
    match issue_type {
        "task" => theme.type_task.clone(),
        "bug" => theme.type_bug.clone(),
        "feature" => theme.type_feature.clone(),
        "epic" => theme.type_epic.clone(),
        "chore" => theme.type_chore.clone(),
        "docs" => theme.type_docs.clone(),
        "question" => theme.type_question.clone(),
        _ => theme.dimmed.clone(),
    }
}

fn build_filters(args: &LintArgs) -> Result<ListFilters> {
    let mut filters = ListFilters {
        include_templates: false,
        ..ListFilters::default()
    };

    if let Some(ref type_str) = args.type_ {
        let issue_type: IssueType = type_str.parse()?;
        filters.types = Some(vec![issue_type]);
    }

    let status_filter = args.status.as_deref().unwrap_or("open").trim();
    if !status_filter.is_empty() && !status_filter.eq_ignore_ascii_case("all") {
        let status: Status = status_filter.parse()?;
        if status.is_terminal() {
            filters.include_closed = true;
        }
        filters.statuses = Some(vec![status]);
    } else if status_filter.eq_ignore_ascii_case("all") {
        filters.include_closed = true;
    }

    Ok(filters)
}

fn resolve_issues(
    storage: &SqliteStorage,
    beads_dir: &Path,
    args: &LintArgs,
    cli: &config::CliOverrides,
) -> Result<Vec<Issue>> {
    let config_layer = config::load_config(beads_dir, Some(storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    let mut issues = Vec::new();
    for id_input in &args.ids {
        let resolution = resolver.resolve(
            id_input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )?;

        match storage.get_issue(&resolution.id)? {
            Some(issue) => issues.push(issue),
            None => eprintln!("Issue not found: {}", resolution.id),
        }
    }

    Ok(issues)
}

fn lint_issues(issues: &[Issue]) -> LintSummary {
    let mut warnings = 0;
    let mut results = Vec::new();

    for issue in issues {
        if let Some(result) = lint_issue(issue) {
            warnings += result.warnings;
            results.push(result);
        }
    }

    LintSummary {
        checked: issues.len(),
        warnings,
        results,
    }
}

fn lint_issue(issue: &Issue) -> Option<LintResult> {
    let required = required_sections(&issue.issue_type);
    if required.is_empty() {
        return None;
    }

    let description = issue.description.as_deref().unwrap_or("");
    let missing = missing_sections(description, required);
    if missing.is_empty() {
        return None;
    }

    Some(LintResult {
        id: issue.id.clone(),
        title: issue.title.clone(),
        issue_type: issue.issue_type.as_str().to_string(),
        warnings: missing.len(),
        missing: missing.into_iter().map(|m| m.heading.to_string()).collect(),
    })
}

const fn required_sections(issue_type: &IssueType) -> &'static [RequiredSection] {
    match issue_type {
        IssueType::Bug => &BUG_SECTIONS,
        IssueType::Task | IssueType::Feature => &TASK_SECTIONS,
        IssueType::Epic => &EPIC_SECTIONS,
        _ => &[],
    }
}

fn missing_sections(description: &str, required: &[RequiredSection]) -> Vec<RequiredSection> {
    let desc_lower = description.to_lowercase();
    let mut missing = Vec::new();

    for section in required {
        let heading_text = strip_heading_prefix(section.heading);
        let heading_lower = heading_text.to_lowercase();
        if !desc_lower.contains(&heading_lower) {
            missing.push(*section);
        }
    }

    missing
}

fn strip_heading_prefix(heading: &str) -> &str {
    let trimmed = heading.trim();
    trimmed
        .strip_prefix("## ")
        .or_else(|| trimmed.strip_prefix("# "))
        .unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_issue(issue_type: IssueType, description: Option<&str>) -> Issue {
        Issue {
            id: "bd-123".to_string(),
            content_hash: None,
            title: "Sample".to_string(),
            description: description.map(str::to_string),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: crate::model::Priority::MEDIUM,
            issue_type,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
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
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_missing_sections_for_bug() {
        let issue = make_issue(IssueType::Bug, Some("Bug report"));
        let result = lint_issue(&issue).expect("lint result");
        assert_eq!(result.warnings, 2);
        assert!(
            result
                .missing
                .contains(&"## Steps to Reproduce".to_string())
        );
        assert!(
            result
                .missing
                .contains(&"## Acceptance Criteria".to_string())
        );
    }

    #[test]
    fn test_required_sections_present_case_insensitive() {
        let description = "## steps to reproduce\n- foo\n# acceptance criteria\n- bar";
        let issue = make_issue(IssueType::Bug, Some(description));
        assert!(lint_issue(&issue).is_none());
    }

    #[test]
    fn test_exit_code_behavior() {
        let issue = make_issue(IssueType::Task, Some("No criteria"));
        let summary = lint_issues(&[issue]);
        assert_eq!(summary.exit_code(true), 0);
        assert_eq!(summary.exit_code(false), 1);
    }
}
