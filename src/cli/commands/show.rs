//! Show command implementation.

use crate::cli::{ShowArgs, resolve_output_format_basic};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{format_priority_label, format_status_icon_colored};
use crate::output::{IssuePanel, OutputContext, OutputMode};
use crate::util::id::IdResolver;
use std::fmt::Write as FmtWrite;

/// Execute the show command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or issues are not found.
pub fn execute(
    args: &ShowArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    let mut target_ids = args.ids.clone();
    if target_ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_ids.push(last_touched);
    }

    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let resolver = IdResolver::with_defaults();
    let use_color = config::should_use_color(&config_layer);
    let output_format = resolve_output_format_basic(args.format, outer_ctx.is_json(), false);
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);

    // How many comments to render. `show` is a summary view, so it bounds
    // comments the same way it already bounds events; the bound is applied
    // to the serialized struct too, so text and JSON agree and neither can
    // drop history silently (`comment_count`/`comments_truncated` say so).
    let comment_limit = crate::cli::commands::comments::parse_comment_limit(args.comments.as_deref())?;

    let mut details_list = Vec::new();
    for id_input in target_ids {
        let resolution = resolver.resolve(
            &id_input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
        )?;

        // Fetch full details including comments and events
        if let Some(mut details) = storage.get_issue_details(&resolution.id, true, false, 10)? {
            details.comments_truncated =
                crate::cli::commands::comments::bound_comments(&mut details.comments, comment_limit);
            details_list.push(details);
        } else {
            return Err(BeadsError::IssueNotFound { id: resolution.id });
        }
    }

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }
    match output_format {
        crate::cli::OutputFormat::Json => {
            ctx.json_pretty(&details_list);
        }
        crate::cli::OutputFormat::Toon => {
            ctx.toon_with_stats(&details_list, args.stats);
        }
        crate::cli::OutputFormat::Text | crate::cli::OutputFormat::Csv => {
            for (i, details) in details_list.iter().enumerate() {
                if i > 0 {
                    println!(); // Separate multiple issues
                }
                if matches!(ctx.mode(), OutputMode::Rich) {
                    let panel = IssuePanel::from_details(details, ctx.theme());
                    panel.print(&ctx, !args.no_wrap);
                } else {
                    print_issue_details(details, use_color);
                }
            }
        }
    }

    Ok(())
}

/// Emit the plain-text detail view.
///
/// `print!`, not `ctx.print`: nothing here is markup, and the console would
/// parse a title reading `fix [bold] rendering` down to `fix  rendering`.
/// Because this sink does not parse markup, nothing written into
/// [`format_issue_details`] may be escaped either.
fn print_issue_details(details: &crate::format::IssueDetails, use_color: bool) {
    let output = format_issue_details(details, use_color);
    print!("{output}");
}

#[allow(clippy::too_many_lines)]
fn format_issue_details(details: &crate::format::IssueDetails, use_color: bool) -> String {
    let mut output = String::new();
    let issue = &details.issue;
    let status_icon = format_status_icon_colored(&issue.status, use_color);
    let priority_label = format_priority_label(&issue.priority, use_color);
    let status_upper = issue.status.as_str().to_uppercase();

    // Match bd format: {status_icon} {id} · {title}   [● {priority} · {STATUS}]
    let _ = writeln!(
        output,
        "{} {} · {}   [● {} · {}]",
        status_icon, issue.id, issue.title, priority_label, status_upper
    );

    // Owner/Type line: Owner: {owner} · Type: {type}
    let owner = issue
        .owner
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()));
    let _ = writeln!(
        output,
        "Owner: {} · Type: {}",
        owner,
        issue.issue_type.as_str()
    );

    // Creator: the agent (or fallback user) that created the issue.
    // Only rendered when present; historical issues predating this
    // field, or beads created before agent-identity resolution
    // existed, simply omit the line rather than showing "unknown".
    if let Some(created_by) = issue.created_by.as_deref() {
        if !created_by.is_empty() {
            let _ = writeln!(output, "Creator: {created_by}");
        }
    }

    // Created/Updated line
    let _ = writeln!(
        output,
        "Created: {} · Updated: {}",
        issue.created_at.format("%Y-%m-%d"),
        issue.updated_at.format("%Y-%m-%d")
    );

    if let Some(assignee) = &issue.assignee {
        let _ = writeln!(output, "Assignee: {assignee}");
    }

    if !details.labels.is_empty() {
        let _ = writeln!(output, "Labels: {}", details.labels.join(", "));
    }

    if let Some(ext_ref) = &issue.external_ref {
        if !ext_ref.is_empty() {
            let _ = writeln!(output, "Ref: {ext_ref}");
        }
    }

    if let Some(due) = &issue.due_at {
        let _ = writeln!(output, "Due: {}", due.format("%Y-%m-%d"));
    }

    if let Some(defer) = &issue.defer_until {
        let _ = writeln!(output, "Deferred until: {}", defer.format("%Y-%m-%d"));
    }

    if let Some(minutes) = issue.estimated_minutes {
        if minutes > 0 {
            let hours = minutes / 60;
            let remaining = minutes % 60;
            if hours > 0 && remaining > 0 {
                let _ = writeln!(output, "Estimate: {hours}h {remaining}m");
            } else if hours > 0 {
                let _ = writeln!(output, "Estimate: {hours}h");
            } else {
                let _ = writeln!(output, "Estimate: {remaining}m");
            }
        }
    }

    if let Some(closed) = &issue.closed_at {
        let reason_str = issue.close_reason.as_deref().unwrap_or("closed");
        let _ = writeln!(
            output,
            "Closed: {} ({})",
            closed.format("%Y-%m-%d"),
            reason_str
        );
    }

    if let Some(desc) = &issue.description {
        output.push('\n');
        let _ = writeln!(output, "{desc}");
    }

    if let Some(design) = &issue.design {
        if !design.is_empty() {
            output.push('\n');
            let _ = writeln!(output, "Design:");
            let _ = writeln!(output, "{design}");
        }
    }

    if let Some(ac) = &issue.acceptance_criteria {
        if !ac.is_empty() {
            output.push('\n');
            let _ = writeln!(output, "Acceptance Criteria:");
            let _ = writeln!(output, "{ac}");
        }
    }

    if let Some(notes) = &issue.notes {
        if !notes.is_empty() {
            output.push('\n');
            let _ = writeln!(output, "Notes:");
            let _ = writeln!(output, "{notes}");
        }
    }

    if !details.dependencies.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependencies:");
        for dep in &details.dependencies {
            let _ = writeln!(output, "  -> {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.dependents.is_empty() {
        output.push('\n');
        let _ = writeln!(output, "Dependents:");
        for dep in &details.dependents {
            let _ = writeln!(output, "  <- {} ({}) - {}", dep.id, dep.dep_type, dep.title);
        }
    }

    if !details.comments.is_empty() || details.comment_count > 0 {
        output.push('\n');
        let _ = writeln!(output, "Comments ({}):", details.comment_count);
        // Say what is hidden BEFORE showing what is not, so the reader
        // learns there is more history above the window rather than
        // discovering it after they have already drawn a conclusion.
        let hidden = details.comment_count.saturating_sub(details.comments.len());
        if details.comments_truncated && hidden > 0 {
            let plural = if hidden == 1 { "" } else { "s" };
            let _ = writeln!(
                output,
                "  ... {hidden} earlier comment{plural} hidden - `bd comments {}` for all",
                issue.id
            );
        }
        for comment in &details.comments {
            // NOT escaped, on purpose. This whole string is emitted by
            // `print_issue_details` with a bare `print!`, which parses
            // nothing, so an escape here reaches the screen as a literal
            // backslash — `bd show` printed `use \[bold] for headings` for
            // one release. Escaping belongs only where a markup parser is
            // waiting (`ctx.print`/`print_data`); see `src/output/mod.rs`
            // for which emit function does what.
            let _ = writeln!(
                output,
                "  [{}] {}: {}",
                comment.created_at.format("%Y-%m-%d %H:%M UTC"),
                comment.author,
                comment.body
            );
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::format_issue_details;
    use crate::format::{IssueDetails, IssueWithDependencyMetadata};
    use crate::model::{Comment, Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use crate::util::id::IdResolver;
    use chrono::{TimeZone, Utc};
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: Some("Test description".to_string()),
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
    fn test_show_retrieves_issue_by_id() {
        init_logging();
        info!("test_show_retrieves_issue_by_id: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue = make_test_issue("bd-001", "Test Issue");
        storage.create_issue(&issue, "tester").unwrap();

        let retrieved = storage.get_issue("bd-001").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "bd-001");
        assert_eq!(retrieved.title, "Test Issue");
        info!("test_show_retrieves_issue_by_id: assertions passed");
    }

    #[test]
    fn test_show_returns_none_for_missing_id() {
        init_logging();
        info!("test_show_returns_none_for_missing_id: starting");
        let storage = SqliteStorage::open_memory().unwrap();

        let retrieved = storage.get_issue("nonexistent").unwrap();
        assert!(retrieved.is_none());
        info!("test_show_returns_none_for_missing_id: assertions passed");
    }

    #[test]
    fn test_show_multiple_issues() {
        init_logging();
        info!("test_show_multiple_issues: starting");
        let mut storage = SqliteStorage::open_memory().unwrap();

        let issue1 = make_test_issue("bd-001", "First Issue");
        let issue2 = make_test_issue("bd-002", "Second Issue");
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        let retrieved1 = storage.get_issue("bd-001").unwrap().unwrap();
        let retrieved2 = storage.get_issue("bd-002").unwrap().unwrap();

        assert_eq!(retrieved1.title, "First Issue");
        assert_eq!(retrieved2.title, "Second Issue");
        info!("test_show_multiple_issues: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization() {
        init_logging();
        info!("test_issue_json_serialization: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string_pretty(&issue).unwrap();

        assert!(json.contains("\"id\": \"bd-001\""));
        assert!(json.contains("\"title\": \"Test Issue\""));
        assert!(json.contains("\"status\": \"open\""));
        info!("test_issue_json_serialization: assertions passed");
    }

    #[test]
    fn test_issue_json_serialization_multiple() {
        init_logging();
        info!("test_issue_json_serialization_multiple: starting");
        let issues = vec![
            make_test_issue("bd-001", "First"),
            make_test_issue("bd-002", "Second"),
        ];

        let json = serde_json::to_string_pretty(&issues).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "bd-001");
        assert_eq!(parsed[1]["id"], "bd-002");
        info!("test_issue_json_serialization_multiple: assertions passed");
    }

    #[test]
    fn test_show_resolves_full_id() {
        init_logging();
        info!("test_show_resolves_full_id: starting");
        let resolver = IdResolver::with_defaults();
        let resolved_id = resolver
            .resolve("bd-abc123", |id| id == "bd-abc123", |_hash| Vec::new())
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_full_id: assertions passed");
    }

    #[test]
    fn test_show_resolves_partial_id() {
        init_logging();
        info!("test_show_resolves_partial_id: starting");
        let resolver = IdResolver::with_defaults();
        let resolved_id = resolver
            .resolve(
                "abc",
                |_id| false,
                |hash| {
                    if hash == "abc" {
                        vec!["bd-abc123".to_string()]
                    } else {
                        Vec::new()
                    }
                },
            )
            .unwrap();
        assert_eq!(resolved_id.id, "bd-abc123");
        info!("test_show_resolves_partial_id: assertions passed");
    }

    #[test]
    fn test_show_not_found_error() {
        init_logging();
        info!("test_show_not_found_error: starting");
        let resolver = IdResolver::with_defaults();
        let result = resolver.resolve("missing", |_id| false, |_hash| Vec::new());
        assert!(result.is_err());
        info!("test_show_not_found_error: assertions passed");
    }

    #[test]
    fn test_show_json_output_shape() {
        init_logging();
        info!("test_show_json_output_shape: starting");
        let issue = make_test_issue("bd-001", "Test Issue");
        let details = IssueDetails {
            issue: issue.clone(),
            labels: vec!["bug".to_string()],
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: Vec::new(),
            events: Vec::new(),
            parent: None,
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&vec![details]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], issue.id);
        assert!(parsed[0]["labels"].is_array());
        assert!(parsed[0]["dependencies"].is_array());
        info!("test_show_json_output_shape: assertions passed");
    }

    #[test]
    fn test_show_text_includes_dependencies_and_comments() {
        init_logging();
        info!("test_show_text_includes_dependencies_and_comments: starting");
        let mut issue = make_test_issue("bd-001", "Test Issue");
        issue.description = None;
        let details = IssueDetails {
            issue,
            labels: Vec::new(),
            dependencies: vec![IssueWithDependencyMetadata {
                id: "bd-002".to_string(),
                title: "Dep".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                dep_type: "blocks".to_string(),
            }],
            dependents: Vec::new(),
            comments: vec![Comment {
                id: 1,
                issue_id: "bd-001".to_string(),
                author: "alice".to_string(),
                body: "Looks good".to_string(),
                created_at: Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 0).unwrap(),
            }],
            events: Vec::new(),
            parent: None,
            comment_count: 1,
            comments_truncated: false,
        };
        let output = format_issue_details(&details, false);
        assert!(output.contains("Dependencies:"));
        assert!(output.contains("-> bd-002 (blocks) - Dep"));
        assert!(output.contains("Comments (1):"));
        assert!(output.contains("alice: Looks good"));
        // Nothing was withheld, so nothing claims anything was.
        assert!(!output.contains("hidden"));
        info!("test_show_text_includes_dependencies_and_comments: assertions passed");
    }

    /// A bounded view must say how much it is NOT showing, and where to get
    /// the rest. Silence here is the failure mode that matters: a reader who
    /// believes they have read the whole thread when they have read the tail.
    #[test]
    fn test_show_text_reports_hidden_comments() {
        init_logging();
        let mut issue = make_test_issue("bd-001", "Test Issue");
        issue.description = None;
        let details = IssueDetails {
            issue,
            comments: vec![Comment {
                id: 9,
                issue_id: "bd-001".to_string(),
                author: "alice".to_string(),
                body: "newest".to_string(),
                created_at: Utc.with_ymd_and_hms(2025, 1, 9, 0, 0, 0).unwrap(),
            }],
            comment_count: 4,
            comments_truncated: true,
            ..Default::default()
        };
        let output = format_issue_details(&details, false);
        assert!(output.contains("Comments (4):"));
        assert!(output.contains("3 earlier comments hidden"));
        // And it names the command that shows the whole log.
        assert!(output.contains("bd comments bd-001"));
        assert!(output.contains("alice: newest"));
    }

    /// Singular/plural on the hidden-count line, because "1 earlier
    /// comments hidden" reads like a bug in the tool.
    #[test]
    fn test_show_text_hidden_comment_singular() {
        init_logging();
        let details = IssueDetails {
            issue: make_test_issue("bd-001", "Test Issue"),
            comments: vec![Comment {
                id: 2,
                issue_id: "bd-001".to_string(),
                author: "bob".to_string(),
                body: "second".to_string(),
                created_at: Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap(),
            }],
            comment_count: 2,
            comments_truncated: true,
            ..Default::default()
        };
        let output = format_issue_details(&details, false);
        assert!(output.contains("1 earlier comment hidden"));
    }

    /// `--comments 0` hides the bodies but must not hide the fact that
    /// history exists: the count still shows, and so does the pointer.
    #[test]
    fn test_show_text_zero_bound_still_reports_count() {
        init_logging();
        let details = IssueDetails {
            issue: make_test_issue("bd-001", "Test Issue"),
            comments: Vec::new(),
            comment_count: 5,
            comments_truncated: true,
            ..Default::default()
        };
        let output = format_issue_details(&details, false);
        assert!(output.contains("Comments (5):"));
        assert!(output.contains("5 earlier comments hidden"));
    }
}
