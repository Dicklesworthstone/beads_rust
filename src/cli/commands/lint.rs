//! Lint command implementation.
//!
//! Checks issues for missing recommended template sections based on issue type.

use super::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale,
    cli_for_routed_workspace, resolve_issue_id,
};
use crate::cli::LintArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    suggestions: Vec<LintSuggestion>,
}

#[derive(Debug, Serialize)]
struct LintSuggestion {
    section: String,
    hint: String,
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
    const fn exit_code(&self, structured: bool) -> i32 {
        if structured || self.warnings == 0 {
            0
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SectionSource {
    Description,
    AcceptanceCriteria,
}

#[derive(Debug, Clone, Copy)]
struct RequiredSection {
    heading: &'static str,
    hint: &'static str,
    source: SectionSource,
}

const BUG_SECTIONS: [RequiredSection; 2] = [
    RequiredSection {
        heading: "## Steps to Reproduce",
        hint: "Describe how to reproduce the bug",
        source: SectionSource::Description,
    },
    RequiredSection {
        heading: "## Acceptance Criteria",
        hint: "Define criteria to verify the fix",
        source: SectionSource::AcceptanceCriteria,
    },
];

const TASK_SECTIONS: [RequiredSection; 1] = [RequiredSection {
    heading: "## Acceptance Criteria",
    hint: "Define criteria to verify completion",
    source: SectionSource::AcceptanceCriteria,
}];

const EPIC_SECTIONS: [RequiredSection; 1] = [RequiredSection {
    heading: "## Success Criteria",
    hint: "Define high-level success criteria",
    source: SectionSource::Description,
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

    let issues = if args.ids.is_empty() {
        let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
        lint_issues_with_storage(args, &storage_ctx.storage)?
    } else {
        resolve_issues(&beads_dir, args, cli)?
    };

    render_lint_output(lint_issues(&issues), ctx);
    Ok(())
}

/// Execute the all-issues lint scan using storage already opened by the caller.
///
/// Returns `Ok(false)` when explicit issue IDs require the normal routed path.
///
/// # Errors
///
/// Returns an error if database access fails or filters are invalid.
pub fn execute_with_storage_ctx(
    args: &LintArgs,
    ctx: &OutputContext,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    if !args.ids.is_empty() {
        return Ok(false);
    }

    let issues = lint_issues_with_storage(args, &storage_ctx.storage)?;
    render_lint_output(lint_issues(&issues), ctx);
    Ok(true)
}

fn lint_issues_with_storage(args: &LintArgs, storage: &SqliteStorage) -> Result<Vec<Issue>> {
    let filters = build_filters(args)?;
    storage.list_lint_issues_with_acceptance(&filters)
}

fn render_lint_output(summary: LintSummary, ctx: &OutputContext) {
    if ctx.is_toon() {
        let output = LintOutput {
            total: summary.warnings,
            issues: summary.results.len(),
            results: summary.results,
        };
        ctx.toon(&output);
        return;
    }

    if ctx.is_json() {
        let output = LintOutput {
            total: summary.warnings,
            issues: summary.results.len(),
            results: summary.results,
        };
        ctx.json_pretty(&output);
        return;
    }

    if ctx.is_quiet() {
        if summary.results.is_empty() {
            return;
        }
        crate::output::record_pending_exit_code(summary.exit_code(false));
        return;
    }

    if ctx.is_rich() {
        render_lint_rich(&summary, ctx);
    } else {
        if summary.results.is_empty() {
            println!(
                "✓ No template warnings found ({} issues checked)",
                summary.checked
            );
            return;
        }

        println!(
            "Template warnings ({} issues, {} warnings):\n",
            summary.results.len(),
            summary.warnings
        );
        for result in &summary.results {
            println!(
                "{} [{}]: {}",
                result.id,
                result.issue_type,
                sanitize_terminal_inline(&result.title)
            );
            for suggestion in &result.suggestions {
                println!("  ⚠ Missing: {} - {}", suggestion.section, suggestion.hint);
            }
            println!();
        }
    }

    // Lint can follow an auto-import owned by main. Let that storage checkpoint
    // before applying the warning status, just like other diagnostic commands.
    if summary.warnings != 0 {
        crate::output::record_pending_exit_code(summary.exit_code(false));
    }
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
                &format!(
                    "{} ({})\n",
                    sanitize_terminal_inline(issue_type),
                    results.len()
                ),
                theme.section.clone(),
            );
            for result in results {
                content.append_styled("- ", theme.warning.clone());
                content.append_styled(&result.id, theme.issue_id.clone());
                content.append(" ");
                content.append_styled(
                    &format!("[{}] ", sanitize_terminal_inline(&result.issue_type)),
                    issue_type_style(theme, &result.issue_type),
                );
                content.append_styled(
                    sanitize_terminal_inline(&result.title).as_ref(),
                    theme.issue_title.clone(),
                );
                content.append("\n");

                for suggestion in &result.suggestions {
                    content.append_styled("    missing: ", theme.dimmed.clone());
                    content.append_styled(&suggestion.section, theme.warning.clone());
                    content.append_styled(" - ", theme.dimmed.clone());
                    content.append_styled(&suggestion.hint, theme.dimmed.clone());
                    content.append("\n");
                }
            }
            content.append("\n");
        }

        content.append_styled(
            "Tip: Set --acceptance-criteria or add the missing sections to issue descriptions.\n",
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
        // bd conformance: CLI rejects custom/unknown types
        if !issue_type.is_standard() {
            return Err(BeadsError::InvalidType {
                issue_type: type_str.clone(),
            });
        }
        filters.types = Some(vec![issue_type]);
    }

    let status_filter = args.status.as_deref().unwrap_or("open").trim();
    if !status_filter.is_empty() && !status_filter.eq_ignore_ascii_case("all") {
        let status: Status = status_filter.parse()?;
        if status.is_terminal() {
            filters.include_closed = true;
        }
        if status == Status::Deferred {
            filters.include_deferred = true;
        }
        filters.statuses = Some(vec![status]);
    } else if status_filter.eq_ignore_ascii_case("all") {
        filters.include_closed = true;
        filters.include_deferred = true;
    }

    Ok(filters)
}

fn resolve_issues(
    beads_dir: &Path,
    args: &LintArgs,
    cli: &config::CliOverrides,
) -> Result<Vec<Issue>> {
    let routed_batches = config::routing::group_issue_inputs_by_route(&args.ids, beads_dir)?;
    let mut issues_by_input = std::collections::HashMap::new();

    for batch in routed_batches {
        let mut batch_cli = routed_cli_for_batch(cli, batch.is_external);
        let routed_write_lock = acquire_routed_workspace_write_lock(
            &batch.beads_dir,
            batch.is_external,
            batch_cli.lock_timeout,
        )?;
        routed_write_lock.mark_cli_write_lock_held(&mut batch_cli);
        let mut storage_ctx = config::open_storage_with_cli(&batch.beads_dir, &batch_cli)?;
        auto_import_storage_ctx_if_stale(&mut storage_ctx, &batch_cli)?;
        let config_layer = storage_ctx.load_config(&batch_cli)?;
        let id_config = config::id_config_from_layer(&config_layer);
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

        let mut resolved_ids = Vec::with_capacity(batch.issue_inputs.len());
        for id_input in &batch.issue_inputs {
            resolved_ids.push(resolve_issue_id(&storage_ctx.storage, &resolver, id_input)?);
        }

        let issues = fetch_issues_in_resolved_order(&storage_ctx.storage, &resolved_ids)?;
        for (input, issue) in batch.issue_inputs.into_iter().zip(issues) {
            issues_by_input.insert(input, issue);
        }
    }

    args.ids
        .iter()
        .map(|input| {
            issues_by_input
                .get(input)
                .cloned()
                .ok_or_else(|| BeadsError::IssueNotFound { id: input.clone() })
        })
        .collect()
}

fn fetch_issues_in_resolved_order(
    storage: &SqliteStorage,
    resolved_ids: &[String],
) -> Result<Vec<Issue>> {
    let issues_by_id = storage
        .get_issues_by_ids(resolved_ids)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect::<std::collections::HashMap<_, _>>();

    resolved_ids
        .iter()
        .map(|id| {
            issues_by_id
                .get(id)
                .cloned()
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.clone() })
        })
        .collect()
}

fn routed_cli_for_batch(cli: &config::CliOverrides, is_external: bool) -> config::CliOverrides {
    cli_for_routed_workspace(cli, is_external)
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

    let missing = missing_sections(issue, required);
    if missing.is_empty() {
        return None;
    }

    let missing_headings = missing
        .iter()
        .map(|section| section.heading.to_string())
        .collect();
    let suggestions = missing
        .iter()
        .map(|section| LintSuggestion {
            section: section.heading.to_string(),
            hint: section.hint.to_string(),
        })
        .collect();

    Some(LintResult {
        id: issue.id.clone(),
        title: issue.title.clone(),
        issue_type: issue.issue_type.as_str().to_string(),
        warnings: missing.len(),
        missing: missing_headings,
        suggestions,
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

fn missing_sections(issue: &Issue, required: &[RequiredSection]) -> Vec<RequiredSection> {
    let description = issue.description.as_deref().unwrap_or("");
    required
        .iter()
        .filter(|section| {
            let field_present = matches!(section.source, SectionSource::AcceptanceCriteria)
                && issue
                    .acceptance_criteria
                    .as_deref()
                    .is_some_and(|criteria| !criteria.trim().is_empty());
            !field_present
                && !description_has_section(description, strip_heading_prefix(section.heading))
        })
        .copied()
        .collect()
}

fn strip_heading_prefix(heading: &str) -> &str {
    let trimmed = heading.trim();
    trimmed
        .strip_prefix("## ")
        .or_else(|| trimmed.strip_prefix("# "))
        .unwrap_or(trimmed)
}

/// Recognize a complete ATX heading, not prose containing a section name.
/// Markdown permits up to three leading spaces and optional closing hashes.
fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let title = rest.trim();
    let without_hashes = title.trim_end_matches('#');
    let title = if without_hashes.ends_with(' ') || without_hashes.ends_with('\t') {
        without_hashes.trim_end()
    } else {
        title
    };
    Some((level, title.strip_suffix(':').unwrap_or(title).trim_end()))
}

/// Legacy descriptions must contain a named heading and a non-blank body.
/// Ignore headings in fenced examples, quotes, and indented code. A section
/// ends at the next heading of the same or a higher level.
fn description_has_section(description: &str, expected: &str) -> bool {
    let mut section_level = None;
    let mut fence: Option<(u8, usize)> = None;

    for raw_line in description.lines() {
        let line = raw_line.trim_start_matches(' ');
        let indented = raw_line.len() - line.len() > 3;
        if let Some((marker, width)) = fence {
            let run = line.bytes().take_while(|byte| *byte == marker).count();
            if !indented && run >= width && line[run..].trim().is_empty() {
                fence = None;
            } else if section_level.is_some() && !line.trim().is_empty() {
                return true;
            }
            continue;
        }

        if !indented && let Some(marker @ (b'`' | b'~')) = line.bytes().next() {
            let width = line.bytes().take_while(|byte| *byte == marker).count();
            if width >= 3 {
                fence = Some((marker, width));
                continue;
            }
        }

        if let Some((level, title)) = markdown_heading(raw_line) {
            if title.eq_ignore_ascii_case(expected) {
                section_level = Some(level);
            } else if section_level.is_some_and(|current| level <= current) {
                section_level = None;
            }
            continue;
        }

        if section_level.is_some() && !line.trim().is_empty() {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    fn make_issue(issue_type: IssueType, description: Option<&str>) -> Issue {
        Issue {
            id: "bd-123".to_string(),
            content_hash: None,
            title: "Sample".to_string(),
            description: description.map(str::to_string),
            design: None,
            acceptance_criteria: None,
            prerequisites: None,
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
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        assert!(result.suggestions.iter().any(|suggestion| {
            suggestion.section == "## Steps to Reproduce"
                && suggestion.hint == "Describe how to reproduce the bug"
        }));
        assert!(result.suggestions.iter().any(|suggestion| {
            suggestion.section == "## Acceptance Criteria"
                && suggestion.hint == "Define criteria to verify the fix"
        }));
    }

    #[test]
    fn test_required_sections_present_case_insensitive() {
        let description = "## steps to reproduce\n- foo\n# acceptance criteria\n- bar";
        let issue = make_issue(IssueType::Bug, Some(description));
        assert!(lint_issue(&issue).is_none());
    }

    #[test]
    fn acceptance_field_satisfies_tasks_and_features_without_description() {
        for issue_type in [IssueType::Task, IssueType::Feature] {
            let mut issue = make_issue(issue_type, None);
            issue.acceptance_criteria = Some(" \n- [ ] the thing works\t".to_string());
            assert!(lint_issue(&issue).is_none(), "{}", issue.issue_type);
            issue.description = Some("## Acceptance Criteria\n".to_string());
            assert!(lint_issue(&issue).is_none(), "field takes precedence");
        }
    }

    #[test]
    fn acceptance_field_does_not_replace_bug_steps_or_epic_success_criteria() {
        let mut bug = make_issue(IssueType::Bug, None);
        bug.acceptance_criteria = Some("The fix works".to_string());
        assert_eq!(
            lint_issue(&bug).unwrap().missing,
            vec!["## Steps to Reproduce"]
        );
        bug.description = Some("## Steps to Reproduce\n1. Trigger the bug".to_string());
        assert!(lint_issue(&bug).is_none());

        let mut epic = make_issue(IssueType::Epic, None);
        epic.acceptance_criteria = Some("The fix works".to_string());
        assert_eq!(
            lint_issue(&epic).unwrap().missing,
            vec!["## Success Criteria"]
        );
        epic.description = Some("## Success Criteria\nThe project succeeds".to_string());
        assert!(lint_issue(&epic).is_none());
    }

    #[test]
    fn blank_acceptance_field_requires_a_real_legacy_section() {
        for criteria in [None, Some(""), Some(" \t\r\n\u{2003}")] {
            let mut issue = make_issue(
                IssueType::Task,
                Some("We will add acceptance criteria later."),
            );
            issue.acceptance_criteria = criteria.map(str::to_string);
            assert_eq!(
                lint_issue(&issue).unwrap().missing,
                vec!["## Acceptance Criteria"],
                "criteria: {criteria:?}"
            );
            issue.description = Some("## Acceptance Criteria\n- [ ] Done".to_string());
            assert!(lint_issue(&issue).is_none(), "criteria: {criteria:?}");
        }
    }

    #[test]
    fn legacy_sections_accept_complete_case_insensitive_markdown_headings() {
        for description in [
            "# acceptance criteria\n- Done",
            "## Acceptance Criteria:\r\n- Done",
            "   ### ACCEPTANCE CRITERIA ###\n- Done",
            "###### Acceptance Criteria\n- Done",
            "##\tAcceptance Criteria\t\n- Done",
            "## Acceptance Criteria\n### Verification\n- Done",
            "## Acceptance Criteria\n```text\nThe thing works\n```",
        ] {
            let issue = make_issue(IssueType::Task, Some(description));
            assert!(lint_issue(&issue).is_none(), "{description:?}");
        }
    }

    #[test]
    fn legacy_sections_reject_prose_lookalikes_and_empty_bodies() {
        for description in [
            "We will add acceptance criteria later.",
            "Acceptance Criteria\n- No heading",
            "Text before ## Acceptance Criteria\n- Done",
            "## Acceptance Criteria backlog\n- Done",
            "## No Acceptance Criteria\n- Done",
            "##Acceptance Criteria\n- Done",
            "####### Acceptance Criteria\n- Done",
            "    ## Acceptance Criteria\n    - Example",
            "> ## Acceptance Criteria\n> - Quoted example",
            "## Acceptance Criteria",
            "## Acceptance Criteria\n \t\n",
            "## Acceptance Criteria\n## Notes\nUnrelated content",
            "## Acceptance Criteria\n# Notes\nUnrelated content",
            "## Acceptance Criteria\n```\n```",
        ] {
            let issue = make_issue(IssueType::Task, Some(description));
            assert_eq!(
                lint_issue(&issue).unwrap().missing,
                vec!["## Acceptance Criteria"],
                "{description:?}"
            );
        }
    }

    #[test]
    fn fenced_examples_do_not_supply_legacy_headings() {
        for description in [
            "```markdown\n## Acceptance Criteria\n- Example\n```",
            "~~~markdown\n## Acceptance Criteria\n- Example\n~~~",
            "````markdown\n```\n## Acceptance Criteria\n- Example\n````",
            "```markdown\n## Acceptance Criteria\n- Unclosed example",
            "```markdown\n```not-a-close\n## Acceptance Criteria\n- Example\n```",
        ] {
            assert!(
                lint_issue(&make_issue(IssueType::Task, Some(description))).is_some(),
                "{description:?}"
            );
        }
        for marker in ["```", "~~~", "````"] {
            let description = format!(
                "{marker}markdown\n## Acceptance Criteria\n- Example\n{marker}\n\n\
                 ## Acceptance Criteria\n- Actual criterion"
            );
            assert!(
                lint_issue(&make_issue(IssueType::Task, Some(&description))).is_none(),
                "{description:?}"
            );
        }
    }

    #[test]
    fn prose_does_not_supply_other_required_sections() {
        let mut bug = make_issue(IssueType::Bug, Some("We need steps to reproduce this bug."));
        bug.acceptance_criteria = Some("The fix works".to_string());
        assert_eq!(
            lint_issue(&bug).unwrap().missing,
            vec!["## Steps to Reproduce"]
        );
        let epic = make_issue(
            IssueType::Epic,
            Some("We will define success criteria later."),
        );
        assert_eq!(
            lint_issue(&epic).unwrap().missing,
            vec!["## Success Criteria"]
        );
    }

    #[test]
    fn lint_issues_with_storage_preserves_acceptance_and_matches_explicit_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).unwrap();
        let mut missing = make_issue(
            IssueType::Task,
            Some("We will add acceptance criteria later."),
        );
        missing.id = "bd-lint-missing".to_string();
        let mut blank = make_issue(IssueType::Task, Some("No criteria supplied"));
        blank.id = "bd-lint-blank".to_string();
        blank.acceptance_criteria = Some(" \t\n".to_string());
        let mut field = make_issue(IssueType::Task, None);
        field.id = "bd-lint-field".to_string();
        field.acceptance_criteria = Some("- [ ] the thing works\r\n".repeat(512));
        field.design = Some("Unused design".repeat(512));
        field.prerequisites = Some("Unused prerequisites".repeat(512));
        field.notes = Some("Unused notes".repeat(512));
        field.owner = Some("owner".to_string());
        field.sender = Some("cli".to_string());
        let mut legacy = make_issue(
            IssueType::Task,
            Some("## Acceptance Criteria\n- Already present"),
        );
        legacy.id = "bd-lint-legacy".to_string();
        for issue in [&mut missing, &mut blank, &mut field, &mut legacy] {
            issue.created_at = now;
            issue.updated_at = now;
            storage.create_issue(issue, "tester").unwrap();
        }

        let scanned = lint_issues_with_storage(&LintArgs::default(), &storage).unwrap();
        assert_eq!(scanned.len(), 4);
        for issue in &scanned {
            assert!(issue.design.is_none());
            assert!(issue.prerequisites.is_none());
            assert!(issue.notes.is_none());
            assert!(issue.owner.is_none());
            assert!(issue.sender.is_none());
        }
        assert_eq!(
            scanned
                .iter()
                .find(|issue| issue.id == field.id)
                .unwrap()
                .acceptance_criteria,
            field.acceptance_criteria
        );
        let ids = scanned
            .iter()
            .map(|issue| issue.id.clone())
            .collect::<Vec<_>>();
        let explicit = fetch_issues_in_resolved_order(&storage, &ids).unwrap();
        let full_field = explicit.iter().find(|issue| issue.id == field.id).unwrap();
        assert_eq!(full_field.design, field.design);
        assert_eq!(full_field.notes, field.notes);
        assert_eq!(
            scanned
                .iter()
                .map(|issue| &issue.acceptance_criteria)
                .collect::<Vec<_>>(),
            explicit
                .iter()
                .map(|issue| &issue.acceptance_criteria)
                .collect::<Vec<_>>()
        );
        let summary = lint_issues(&scanned);
        assert_eq!(summary.checked, 4);
        assert_eq!(summary.warnings, 2);
        assert_eq!(summary.results.len(), 2);
        assert!(summary.results.iter().any(|result| result.id == missing.id));
        assert!(summary.results.iter().any(|result| result.id == blank.id));
        assert_eq!(
            serde_json::to_value(summary.results).unwrap(),
            serde_json::to_value(lint_issues(&explicit).results).unwrap()
        );
    }

    #[test]
    fn acceptance_projection_preserves_lint_filters_and_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        for (index, (issue_type, status, template, criteria)) in [
            (
                IssueType::Task,
                Status::Open,
                false,
                Some("- [ ] Field criterion"),
            ),
            (IssueType::Bug, Status::Open, false, None),
            (IssueType::Feature, Status::Closed, false, Some("")),
            (IssueType::Task, Status::Deferred, false, Some(" \t\n")),
            (IssueType::Task, Status::Open, true, Some("Template only")),
        ]
        .into_iter()
        .enumerate()
        {
            let mut issue = make_issue(issue_type, Some("No template sections"));
            issue.id = format!("bd-lint-filter-{index}");
            issue.status = status;
            issue.is_template = template;
            issue.acceptance_criteria = criteria.map(str::to_string);
            if issue.status == Status::Closed {
                issue.closed_at = Some(issue.updated_at);
            }
            storage.create_issue(&issue, "tester").unwrap();
        }

        for (type_filter, status_filter, expected_count) in [
            (None, None, 2),
            (Some("task"), None, 1),
            (Some("epic"), None, 0),
            (None, Some("all"), 4),
            (None, Some("closed"), 1),
            (None, Some("deferred"), 1),
        ] {
            let args = LintArgs {
                type_: type_filter.map(str::to_string),
                status: status_filter.map(str::to_string),
                ..LintArgs::default()
            };
            let expected = storage.list_issues(&build_filters(&args).unwrap()).unwrap();
            let actual = lint_issues_with_storage(&args, &storage).unwrap();
            assert_eq!(
                actual.len(),
                expected_count,
                "{type_filter:?}/{status_filter:?}"
            );
            assert_eq!(
                actual
                    .iter()
                    .map(|issue| (&issue.id, &issue.acceptance_criteria))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|issue| (&issue.id, &issue.acceptance_criteria))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                serde_json::to_value(lint_issues(&actual).results).unwrap(),
                serde_json::to_value(lint_issues(&expected).results).unwrap()
            );
        }
    }

    #[test]
    fn fetch_issues_in_resolved_order_preserves_duplicate_ids() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue(IssueType::Bug, Some("Bug report")), "tester")
            .expect("create issue");

        let duplicate_ids = vec!["bd-123".to_string(), "bd-123".to_string()];
        let issues =
            fetch_issues_in_resolved_order(&storage, &duplicate_ids).expect("duplicate lookup");

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].id, "bd-123");
        assert_eq!(issues[1].id, "bd-123");
    }

    #[test]
    fn test_exit_code_behavior() {
        let issue = make_issue(IssueType::Task, Some("No criteria"));
        let summary = lint_issues(&[issue]);
        assert_eq!(summary.exit_code(true), 0);
        assert_eq!(summary.exit_code(false), 1);
    }
}
