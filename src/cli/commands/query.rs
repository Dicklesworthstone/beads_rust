//! Saved query command implementation.
//!
//! Provides named, reusable filters for issue listing.

use crate::cli::{ListArgs, QueryCommands, QueryDeleteArgs, QueryRunArgs, QuerySaveArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::{OutputContext, OutputMode};
use chrono::{DateTime, Utc};
use rich_rust::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Prefix for saved query keys in the config table.
const QUERY_KEY_PREFIX: &str = "saved_query:";

/// A saved query stored in the config table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQuery {
    /// Query name
    pub name: String,
    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When the query was created
    pub created_at: DateTime<Utc>,
    /// Filter configuration (stored as serializable form)
    pub filters: SavedFilters,
}

/// Serializable filter configuration.
/// Mirrors `ListArgs` but with serializable types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SavedFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unassigned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub id: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_min: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_max: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_contains: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reverse: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deferred: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overdue: bool,
}

/// Helper for serde `skip_serializing_if` (requires reference signature).
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}

impl From<&ListArgs> for SavedFilters {
    fn from(args: &ListArgs) -> Self {
        Self {
            status: args.status.clone(),
            type_: args.type_.clone(),
            assignee: args.assignee.clone(),
            unassigned: args.unassigned,
            id: args.id.clone(),
            label: args.label.clone(),
            label_any: args.label_any.clone(),
            priority: args.priority.clone(),
            priority_min: args.priority_min,
            priority_max: args.priority_max,
            title_contains: args.title_contains.clone(),
            desc_contains: args.desc_contains.clone(),
            notes_contains: args.notes_contains.clone(),
            all: args.all,
            limit: args.limit,
            offset: args.offset,
            sort: args.sort.clone(),
            reverse: args.reverse,
            deferred: args.deferred,
            overdue: args.overdue,
        }
    }
}

impl SavedFilters {
    /// Convert saved filters to `ListArgs`.
    #[must_use]
    pub fn to_list_args(&self) -> ListArgs {
        ListArgs {
            status: self.status.clone(),
            type_: self.type_.clone(),
            assignee: self.assignee.clone(),
            unassigned: self.unassigned,
            id: self.id.clone(),
            label: self.label.clone(),
            label_any: self.label_any.clone(),
            priority: self.priority.clone(),
            priority_min: self.priority_min,
            priority_max: self.priority_max,
            title_contains: self.title_contains.clone(),
            desc_contains: self.desc_contains.clone(),
            notes_contains: self.notes_contains.clone(),
            all: self.all,
            limit: self.limit,
            offset: self.offset,
            sort: self.sort.clone(),
            reverse: self.reverse,
            deferred: self.deferred,
            overdue: self.overdue,
            // Output-related fields use defaults
            long: false,
            pretty: false,
            tree: false,
            wrap: false,
            format: None,
            stats: false,
            fields: None,
        }
    }

    /// Merge CLI args onto saved filters. CLI takes precedence for non-empty values.
    #[must_use]
    pub fn merge_with_cli(&self, cli: &ListArgs) -> ListArgs {
        let base = self.to_list_args();
        let (assignee, unassigned) = match (&cli.assignee, cli.unassigned) {
            (Some(assignee), false) => (Some(assignee.clone()), false),
            (None, true) => (None, true),
            // Preserve explicitly contradictory CLI input; the caller asked for both.
            (Some(assignee), true) => (Some(assignee.clone()), true),
            (None, false) => (base.assignee.clone(), base.unassigned),
        };
        ListArgs {
            // Vec fields: CLI overrides if non-empty
            status: if cli.status.is_empty() {
                base.status
            } else {
                cli.status.clone()
            },
            type_: if cli.type_.is_empty() {
                base.type_
            } else {
                cli.type_.clone()
            },
            id: if cli.id.is_empty() {
                base.id
            } else {
                cli.id.clone()
            },
            label: if cli.label.is_empty() {
                base.label
            } else {
                cli.label.clone()
            },
            label_any: if cli.label_any.is_empty() {
                base.label_any
            } else {
                cli.label_any.clone()
            },
            priority: if cli.priority.is_empty() {
                base.priority
            } else {
                cli.priority.clone()
            },
            // Option fields: CLI overrides if Some
            assignee,
            priority_min: cli.priority_min.or(base.priority_min),
            priority_max: cli.priority_max.or(base.priority_max),
            title_contains: cli.title_contains.clone().or(base.title_contains),
            desc_contains: cli.desc_contains.clone().or(base.desc_contains),
            notes_contains: cli.notes_contains.clone().or(base.notes_contains),
            limit: cli.limit.or(base.limit),
            offset: cli.offset.or(base.offset),
            sort: cli.sort.clone().or(base.sort),
            // Bool fields: CLI true overrides saved
            unassigned,
            all: cli.all || base.all,
            reverse: cli.reverse || base.reverse,
            deferred: cli.deferred || base.deferred,
            overdue: cli.overdue || base.overdue,
            // Output fields from CLI only
            long: cli.long,
            pretty: cli.pretty,
            tree: cli.tree,
            wrap: cli.wrap,
            format: cli.format,
            stats: cli.stats,
            fields: cli.fields.clone(),
        }
    }
}

/// JSON output for query list.
#[derive(Serialize)]
struct QueryListOutput {
    queries: Vec<QueryListItem>,
    count: usize,
}

/// Single query in list output.
#[derive(Serialize)]
struct QueryListItem {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    created_at: String,
    filters: SavedFilters,
}

/// JSON output for query save/delete.
#[derive(Serialize)]
struct QueryActionOutput {
    status: String,
    name: String,
    action: String,
}

/// Execute the query command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(
    command: &QueryCommands,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_with_storage_ctx(command, cli, ctx, &mut storage_ctx)
}

/// Execute the query command using storage already opened by the caller.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute_with_storage_ctx(
    command: &QueryCommands,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    storage_ctx: &mut config::OpenStorageResult,
) -> Result<()> {
    ensure_query_storage_available(storage_ctx)?;

    match command {
        QueryCommands::Save(args) => query_save(args, &mut storage_ctx.storage, ctx),
        QueryCommands::Run(args) => query_run(args, storage_ctx, cli, ctx),
        QueryCommands::List => query_list(&storage_ctx.storage, ctx),
        QueryCommands::Delete(args) => query_delete(args, &mut storage_ctx.storage, ctx),
    }
}

fn ensure_query_storage_available(storage_ctx: &config::OpenStorageResult) -> Result<()> {
    if storage_ctx.no_db {
        return Err(BeadsError::validation(
            "no_db",
            "--no-db is not supported for query commands because saved queries are stored only in the database",
        ));
    }

    Ok(())
}

fn query_save(
    args: &QuerySaveArgs,
    storage: &mut crate::storage::SqliteStorage,
    ctx: &OutputContext,
) -> Result<()> {
    let name = args.name.trim();

    // Validate name
    if name.is_empty() {
        return Err(BeadsError::validation("name", "Query name cannot be empty"));
    }
    if name.contains(':') || name.contains('/') {
        return Err(BeadsError::validation(
            "name",
            "Query name cannot contain ':' or '/'",
        ));
    }

    let key = format!("{QUERY_KEY_PREFIX}{name}");

    // Check if query already exists
    if storage.get_config(&key)?.is_some() {
        return Err(BeadsError::validation(
            "name",
            format!(
                "Query '{}' already exists. Delete it first to replace.",
                query_display_text(name)
            ),
        ));
    }

    super::list::validate_list_args(&args.filters)?;

    let saved_query = SavedQuery {
        name: name.to_string(),
        description: args.description.clone(),
        created_at: Utc::now(),
        filters: SavedFilters::from(&args.filters),
    };

    let value = serde_json::to_string(&saved_query)?;
    storage.set_config(&key, &value)?;

    info!(name, "Saved query created");

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "saved".to_string(),
        };
        ctx.json_pretty(&output);
    } else if ctx.is_toon() {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "saved".to_string(),
        };
        ctx.toon(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_query_save_rich(name, args.description.as_deref(), ctx);
    } else {
        println!("{}", format_query_saved_line(name));
    }

    Ok(())
}

fn query_run(
    args: &QueryRunArgs,
    storage_ctx: &config::OpenStorageResult,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let name = args.name.trim();
    let key = format!("{QUERY_KEY_PREFIX}{name}");

    let value = storage_ctx
        .storage
        .get_config(&key)?
        .ok_or_else(|| BeadsError::validation("query", format_query_not_found_message(name)))?;

    let saved_query: SavedQuery = serde_json::from_str(&value).map_err(|e| {
        BeadsError::validation("saved_query", format!("Invalid saved query format: {e}"))
    })?;

    debug!(name, "Loaded saved query");

    // Merge saved filters with CLI overrides
    let merged_args = saved_query.filters.merge_with_cli(&args.filters);

    debug!(?merged_args, "Merged filters");

    // Execute list command with merged args
    super::list::execute_with_storage(&merged_args, cli, ctx, storage_ctx)
}

fn query_list(storage: &crate::storage::SqliteStorage, ctx: &OutputContext) -> Result<()> {
    let all_config = storage.get_all_config()?;

    let mut queries: Vec<QueryListItem> = Vec::new();

    for (key, value) in &all_config {
        if let Some(name) = key.strip_prefix(QUERY_KEY_PREFIX) {
            match serde_json::from_str::<SavedQuery>(value) {
                Ok(saved) => {
                    queries.push(QueryListItem {
                        name: name.to_string(),
                        description: saved.description,
                        created_at: saved.created_at.to_rfc3339(),
                        filters: saved.filters,
                    });
                }
                Err(e) => {
                    debug!(name, error = %e, "Skipping malformed saved query");
                }
            }
        }
    }

    // Sort by name
    queries.sort_by(|a, b| a.name.cmp(&b.name));

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_array_count("queries", queries.iter(), "count", queries.len());
    } else if ctx.is_toon() {
        let output = QueryListOutput {
            count: queries.len(),
            queries,
        };
        ctx.toon(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_query_list_rich(&queries, ctx);
    } else if queries.is_empty() {
        println!("No saved queries");
    } else {
        println!("Saved queries:");
        for q in &queries {
            println!("  {}", format_query_list_plain_entry(q));
        }
        println!("\n{} query(ies) total", queries.len());
    }

    Ok(())
}

fn query_delete(
    args: &QueryDeleteArgs,
    storage: &mut crate::storage::SqliteStorage,
    ctx: &OutputContext,
) -> Result<()> {
    let name = args.name.trim();
    let key = format!("{QUERY_KEY_PREFIX}{name}");

    let deleted = storage.delete_config(&key)?;

    if !deleted {
        return Err(BeadsError::validation(
            "query",
            format_query_not_found_message(name),
        ));
    }

    info!(name, "Saved query deleted");

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "deleted".to_string(),
        };
        ctx.json_pretty(&output);
    } else if ctx.is_toon() {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "deleted".to_string(),
        };
        ctx.toon(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_query_delete_rich(name, ctx);
    } else {
        println!("{}", format_query_deleted_line(name));
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

fn query_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn format_query_saved_line(name: &str) -> String {
    format!("Saved query '{}'", query_display_text(name))
}

fn format_query_deleted_line(name: &str) -> String {
    format!("Deleted query '{}'", query_display_text(name))
}

fn format_query_not_found_message(name: &str) -> String {
    format!("Query '{}' not found", query_display_text(name))
}

fn format_query_list_plain_entry(query: &QueryListItem) -> String {
    let name = query_display_text(&query.name);
    match query.description.as_deref() {
        Some(desc) if !desc.is_empty() => format!("{} - {}", name, query_display_text(desc)),
        _ => name,
    }
}

/// Render query save result with rich formatting.
fn render_query_save_rich(name: &str, description: Option<&str>, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("\u{2713} ", theme.success.clone());
    content.append_styled("Saved query ", theme.success.clone());
    content.append_styled(&query_display_text(name), theme.emphasis.clone());
    content.append("\n");
    if let Some(desc) = description {
        content.append_styled("  ", theme.dimmed.clone());
        content.append_styled(&query_display_text(desc), theme.dimmed.clone());
        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Query Saved", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render query list with rich formatting.
fn render_query_list_rich(queries: &[QueryListItem], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if queries.is_empty() {
        content.append_styled("No saved queries\n", theme.dimmed.clone());
    } else {
        // Find longest name for alignment
        let rendered_queries: Vec<(String, Option<String>)> = queries
            .iter()
            .map(|query| {
                (
                    query_display_text(&query.name),
                    query.description.as_deref().map(query_display_text),
                )
            })
            .collect();
        let max_name_len = rendered_queries
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);

        for (mut name, description) in rendered_queries {
            let padding = max_name_len.saturating_sub(name.len());
            name.reserve(padding);
            for _ in 0..padding {
                name.push(' ');
            }
            content.append_styled(&name, theme.emphasis.clone());
            content.append("  ");
            if let Some(ref desc) = description {
                content.append_styled(desc, theme.dimmed.clone());
            } else {
                content.append_styled("(no description)", theme.dimmed.clone());
            }
            content.append("\n");
        }

        content.append("\n");
        content.append_styled(
            &format!("{} query(ies) total", queries.len()),
            theme.dimmed.clone(),
        );
        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Saved Queries", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render query delete result with rich formatting.
fn render_query_delete_rich(name: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("\u{2713} ", theme.success.clone());
    content.append_styled("Deleted query ", theme.success.clone());
    content.append_styled(&query_display_text(name), theme.emphasis.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Query Deleted", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{DEFAULT_LIST_LIMIT, OutputFormat};
    use crate::storage::SqliteStorage;

    #[test]
    fn test_saved_filters_from_list_args() {
        let args = ListArgs {
            status: vec!["open".to_string()],
            type_: vec!["bug".to_string()],
            assignee: Some("alice".to_string()),
            priority: vec!["1".to_string(), "2".to_string()],
            offset: Some(3),
            ..Default::default()
        };

        let filters = SavedFilters::from(&args);
        assert_eq!(filters.status, vec!["open"]);
        assert_eq!(filters.type_, vec!["bug"]);
        assert_eq!(filters.assignee, Some("alice".to_string()));
        assert_eq!(filters.priority, vec!["1", "2"]);
        assert_eq!(filters.offset, Some(3));
    }

    #[test]
    fn test_saved_filters_to_list_args() {
        let filters = SavedFilters {
            status: vec!["open".to_string()],
            assignee: Some("bob".to_string()),
            all: true,
            offset: Some(3),
            ..Default::default()
        };

        let args = filters.to_list_args();
        assert_eq!(args.status, vec!["open"]);
        assert_eq!(args.assignee, Some("bob".to_string()));
        assert!(args.all);
        assert_eq!(args.offset, Some(3));
    }

    #[test]
    fn test_merge_cli_overrides_saved() {
        let saved = SavedFilters {
            status: vec!["open".to_string()],
            assignee: Some("alice".to_string()),
            limit: Some(10),
            ..Default::default()
        };

        let cli = ListArgs {
            status: vec!["closed".to_string()], // Override
            assignee: None,                     // Keep saved
            limit: Some(20),                    // Override
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.status, vec!["closed"]); // CLI wins
        assert_eq!(merged.assignee, Some("alice".to_string())); // Saved retained
        assert_eq!(merged.limit, Some(20)); // CLI wins
    }

    #[test]
    fn test_merge_preserves_saved_limit_when_cli_limit_absent() {
        let saved = SavedFilters {
            limit: Some(10),
            ..Default::default()
        };

        let cli = ListArgs::default();

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.limit, Some(10));
    }

    #[test]
    fn test_merge_preserves_saved_unlimited_limit_when_cli_limit_absent() {
        let saved = SavedFilters {
            limit: Some(0),
            ..Default::default()
        };

        let cli = ListArgs::default();

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.limit, Some(0));
    }

    #[test]
    fn test_merge_keeps_limit_absent_when_saved_and_cli_omit_limit() {
        let saved = SavedFilters::default();
        let cli = ListArgs::default();

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.limit, None);
    }

    #[test]
    fn test_merge_cli_limit_can_override_saved_with_default_limit_value() {
        let saved = SavedFilters {
            limit: Some(10),
            ..Default::default()
        };
        let cli = ListArgs {
            limit: Some(DEFAULT_LIST_LIMIT),
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.limit, Some(DEFAULT_LIST_LIMIT));
    }

    #[test]
    fn test_merge_empty_cli_keeps_saved() {
        let saved = SavedFilters {
            status: vec!["open".to_string()],
            type_: vec!["bug".to_string()],
            limit: Some(50),
            ..Default::default()
        };

        let cli = ListArgs::default();

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.status, vec!["open"]);
        assert_eq!(merged.type_, vec!["bug"]);
        assert_eq!(merged.limit, Some(50));
    }

    #[test]
    fn test_saved_query_serialization() {
        let query = SavedQuery {
            name: "my-bugs".to_string(),
            description: Some("All open bugs".to_string()),
            created_at: Utc::now(),
            filters: SavedFilters {
                status: vec!["open".to_string()],
                type_: vec!["bug".to_string()],
                ..Default::default()
            },
        };

        let json = serde_json::to_string(&query).unwrap();
        let parsed: SavedQuery = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "my-bugs");
        assert_eq!(parsed.description, Some("All open bugs".to_string()));
        assert_eq!(parsed.filters.status, vec!["open"]);
        assert_eq!(parsed.filters.type_, vec!["bug"]);
    }

    #[test]
    fn query_human_output_helpers_escape_terminal_controls() {
        let item = QueryListItem {
            name: "triage\x1b[2J".to_string(),
            description: Some("desc\x07\rnext".to_string()),
            created_at: Utc::now().to_rfc3339(),
            filters: SavedFilters::default(),
        };

        let rendered_saved = format_query_saved_line(&item.name);
        let rendered_deleted = format_query_deleted_line(&item.name);
        let rendered_not_found = format_query_not_found_message(&item.name);
        let rendered_entry = format_query_list_plain_entry(&item);
        assert!(rendered_entry.contains("\\u{7}"));
        assert!(rendered_entry.contains("\\rnext"));

        for line in [
            &rendered_saved,
            &rendered_deleted,
            &rendered_not_found,
            &rendered_entry,
        ] {
            assert!(!line.chars().any(char::is_control));
            assert!(line.contains("\\u{1b}[2J"));
        }
    }

    // ============================================================
    // Additional tests for comprehensive query module coverage
    // ============================================================

    #[test]
    fn test_saved_filters_default() {
        let filters = SavedFilters::default();
        assert!(filters.status.is_empty());
        assert!(filters.type_.is_empty());
        assert!(filters.assignee.is_none());
        assert!(!filters.unassigned);
        assert!(filters.id.is_empty());
        assert!(filters.label.is_empty());
        assert!(filters.label_any.is_empty());
        assert!(filters.priority.is_empty());
        assert!(filters.priority_min.is_none());
        assert!(filters.priority_max.is_none());
        assert!(filters.title_contains.is_none());
        assert!(filters.desc_contains.is_none());
        assert!(filters.notes_contains.is_none());
        assert!(!filters.all);
        assert!(filters.limit.is_none());
        assert!(filters.offset.is_none());
        assert!(filters.sort.is_none());
        assert!(!filters.reverse);
        assert!(!filters.deferred);
        assert!(!filters.overdue);
    }

    #[test]
    fn test_is_false_helper() {
        assert!(is_false(&false));
        assert!(!is_false(&true));
    }

    #[test]
    fn test_merge_boolean_fields_cli_true_wins() {
        let saved = SavedFilters {
            unassigned: false,
            all: false,
            reverse: false,
            deferred: false,
            overdue: false,
            ..Default::default()
        };

        let cli = ListArgs {
            unassigned: true,
            all: true,
            reverse: true,
            deferred: true,
            overdue: true,
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        assert!(merged.unassigned);
        assert!(merged.all);
        assert!(merged.reverse);
        assert!(merged.deferred);
        assert!(merged.overdue);
    }

    #[test]
    fn test_merge_boolean_fields_saved_true_preserved() {
        let saved = SavedFilters {
            unassigned: true,
            all: true,
            reverse: true,
            deferred: true,
            overdue: true,
            ..Default::default()
        };

        let cli = ListArgs {
            unassigned: false,
            all: false,
            reverse: false,
            deferred: false,
            overdue: false,
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        // Boolean merge is OR: saved true + cli false = true
        assert!(merged.unassigned);
        assert!(merged.all);
        assert!(merged.reverse);
        assert!(merged.deferred);
        assert!(merged.overdue);
    }

    #[test]
    fn test_merge_assignee_override_clears_saved_unassigned() {
        let saved = SavedFilters {
            unassigned: true,
            ..Default::default()
        };

        let cli = ListArgs {
            assignee: Some("alice".to_string()),
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.assignee, Some("alice".to_string()));
        assert!(!merged.unassigned);
    }

    #[test]
    fn test_merge_unassigned_override_clears_saved_assignee() {
        let saved = SavedFilters {
            assignee: Some("alice".to_string()),
            ..Default::default()
        };

        let cli = ListArgs {
            unassigned: true,
            ..Default::default()
        };

        let merged = saved.merge_with_cli(&cli);
        assert!(merged.assignee.is_none());
        assert!(merged.unassigned);
    }

    #[test]
    fn test_merge_all_vec_fields() {
        let saved = SavedFilters {
            status: vec!["open".to_string()],
            type_: vec!["bug".to_string()],
            id: vec!["abc".to_string()],
            label: vec!["urgent".to_string()],
            label_any: vec!["maybe".to_string()],
            priority: vec!["1".to_string(), "2".to_string()],
            ..Default::default()
        };

        // CLI with all empty vecs - saved values preserved
        let cli = ListArgs::default();
        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.status, vec!["open"]);
        assert_eq!(merged.type_, vec!["bug"]);
        assert_eq!(merged.id, vec!["abc"]);
        assert_eq!(merged.label, vec!["urgent"]);
        assert_eq!(merged.label_any, vec!["maybe"]);
        assert_eq!(merged.priority, vec!["1", "2"]);

        // CLI with non-empty vecs - cli values win
        let cli2 = ListArgs {
            status: vec!["closed".to_string()],
            type_: vec!["feature".to_string()],
            id: vec!["xyz".to_string()],
            label: vec!["low".to_string()],
            label_any: vec!["high".to_string()],
            priority: vec!["3".to_string()],
            ..Default::default()
        };
        let merged2 = saved.merge_with_cli(&cli2);
        assert_eq!(merged2.status, vec!["closed"]);
        assert_eq!(merged2.type_, vec!["feature"]);
        assert_eq!(merged2.id, vec!["xyz"]);
        assert_eq!(merged2.label, vec!["low"]);
        assert_eq!(merged2.label_any, vec!["high"]);
        assert_eq!(merged2.priority, vec!["3"]);
    }

    #[test]
    fn test_merge_option_fields() {
        let saved = SavedFilters {
            assignee: Some("alice".to_string()),
            priority_min: Some(1),
            priority_max: Some(3),
            title_contains: Some("bug".to_string()),
            desc_contains: Some("error".to_string()),
            notes_contains: Some("important".to_string()),
            limit: Some(100),
            offset: Some(5),
            sort: Some("priority".to_string()),
            ..Default::default()
        };

        // CLI with None values - saved preserved
        let cli = ListArgs::default();
        let merged = saved.merge_with_cli(&cli);
        assert_eq!(merged.assignee, Some("alice".to_string()));
        assert_eq!(merged.priority_min, Some(1));
        assert_eq!(merged.priority_max, Some(3));
        assert_eq!(merged.title_contains, Some("bug".to_string()));
        assert_eq!(merged.desc_contains, Some("error".to_string()));
        assert_eq!(merged.notes_contains, Some("important".to_string()));
        assert_eq!(merged.limit, Some(100));
        assert_eq!(merged.offset, Some(5));
        assert_eq!(merged.sort, Some("priority".to_string()));

        // CLI with Some values - cli wins
        let cli2 = ListArgs {
            assignee: Some("bob".to_string()),
            priority_min: Some(2),
            priority_max: Some(4),
            title_contains: Some("feature".to_string()),
            desc_contains: Some("new".to_string()),
            notes_contains: Some("todo".to_string()),
            limit: Some(50),
            offset: Some(2),
            sort: Some("updated".to_string()),
            ..Default::default()
        };
        let merged2 = saved.merge_with_cli(&cli2);
        assert_eq!(merged2.assignee, Some("bob".to_string()));
        assert_eq!(merged2.priority_min, Some(2));
        assert_eq!(merged2.priority_max, Some(4));
        assert_eq!(merged2.title_contains, Some("feature".to_string()));
        assert_eq!(merged2.desc_contains, Some("new".to_string()));
        assert_eq!(merged2.notes_contains, Some("todo".to_string()));
        assert_eq!(merged2.limit, Some(50));
        assert_eq!(merged2.offset, Some(2));
        assert_eq!(merged2.sort, Some("updated".to_string()));
    }

    #[test]
    fn test_saved_query_without_description() {
        let query = SavedQuery {
            name: "quick".to_string(),
            description: None,
            created_at: Utc::now(),
            filters: SavedFilters::default(),
        };

        let json = serde_json::to_string(&query).unwrap();
        // description should be skipped when None
        assert!(!json.contains("description"));

        let parsed: SavedQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "quick");
        assert!(parsed.description.is_none());
    }

    #[test]
    fn test_saved_filters_serialization_skips_defaults() {
        let filters = SavedFilters::default();
        let json = serde_json::to_string(&filters).unwrap();

        // Default values should be skipped by serde
        assert!(!json.contains("\"status\""));
        assert!(!json.contains("\"type_\""));
        assert!(!json.contains("\"assignee\""));
        assert!(!json.contains("\"unassigned\""));
        assert!(!json.contains("\"all\""));
        assert!(!json.contains("\"reverse\""));

        // Should be minimal JSON
        assert_eq!(json, "{}");
    }

    #[test]
    fn test_saved_filters_roundtrip_all_fields() {
        let filters = SavedFilters {
            status: vec!["open".to_string(), "in_progress".to_string()],
            type_: vec!["bug".to_string(), "feature".to_string()],
            assignee: Some("charlie".to_string()),
            unassigned: false,
            id: vec!["id1".to_string(), "id2".to_string()],
            label: vec!["urgent".to_string(), "backend".to_string()],
            label_any: vec!["optional".to_string()],
            priority: vec!["0".to_string(), "1".to_string(), "2".to_string()],
            priority_min: Some(0),
            priority_max: Some(2),
            title_contains: Some("search term".to_string()),
            desc_contains: Some("description search".to_string()),
            notes_contains: Some("notes search".to_string()),
            all: true,
            limit: Some(25),
            offset: Some(5),
            sort: Some("created".to_string()),
            reverse: true,
            deferred: true,
            overdue: true,
        };

        let json = serde_json::to_string(&filters).unwrap();
        let parsed: SavedFilters = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.status, filters.status);
        assert_eq!(parsed.type_, filters.type_);
        assert_eq!(parsed.assignee, filters.assignee);
        assert_eq!(parsed.unassigned, filters.unassigned);
        assert_eq!(parsed.id, filters.id);
        assert_eq!(parsed.label, filters.label);
        assert_eq!(parsed.label_any, filters.label_any);
        assert_eq!(parsed.priority, filters.priority);
        assert_eq!(parsed.priority_min, filters.priority_min);
        assert_eq!(parsed.priority_max, filters.priority_max);
        assert_eq!(parsed.title_contains, filters.title_contains);
        assert_eq!(parsed.desc_contains, filters.desc_contains);
        assert_eq!(parsed.notes_contains, filters.notes_contains);
        assert_eq!(parsed.all, filters.all);
        assert_eq!(parsed.limit, filters.limit);
        assert_eq!(parsed.offset, filters.offset);
        assert_eq!(parsed.sort, filters.sort);
        assert_eq!(parsed.reverse, filters.reverse);
        assert_eq!(parsed.deferred, filters.deferred);
        assert_eq!(parsed.overdue, filters.overdue);
    }

    #[test]
    fn test_to_list_args_preserves_output_defaults() {
        let filters = SavedFilters {
            status: vec!["open".to_string()],
            ..Default::default()
        };

        let args = filters.to_list_args();

        // Output-related fields should have defaults
        assert!(!args.long);
        assert!(!args.pretty);
        assert!(args.format.is_none());
        assert!(args.fields.is_none());
    }

    #[test]
    fn test_from_list_args_ignores_output_fields() {
        let args = ListArgs {
            status: vec!["open".to_string()],
            long: true,
            pretty: true,
            format: Some(OutputFormat::Json),
            fields: Some("id,title".to_string()),
            ..Default::default()
        };

        let filters = SavedFilters::from(&args);

        // Saved filters should not contain output-related fields
        // They're simply not part of the SavedFilters struct
        assert_eq!(filters.status, vec!["open"]);
    }

    #[test]
    fn test_query_save_rejects_invalid_filters_without_persisting() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let ctx = OutputContext::from_output_format(OutputFormat::Text, true, true);
        let args = QuerySaveArgs {
            name: "broken".to_string(),
            description: None,
            filters: ListArgs {
                sort: Some("nonsense".to_string()),
                ..Default::default()
            },
        };

        let err = query_save(&args, &mut storage, &ctx).expect_err("invalid query save");
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "sort"));
        assert_eq!(storage.get_config("saved_query:broken").unwrap(), None);
    }
}
