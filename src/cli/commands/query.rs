//! Saved query command implementation.
//!
//! Provides named, reusable filters for issue listing.

use crate::cli::{
    ListArgs, OutputFormat, QueryCommands, QueryDeleteArgs, QueryRunArgs, QuerySaveArgs,
};
use crate::config;
use crate::error::{BeadsError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
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
            sort: self.sort.clone(),
            reverse: self.reverse,
            deferred: self.deferred,
            overdue: self.overdue,
            // Output-related fields use defaults
            long: false,
            pretty: false,
            format: OutputFormat::default(),
            fields: None,
        }
    }

    /// Merge CLI args onto saved filters. CLI takes precedence for non-empty values.
    #[must_use]
    pub fn merge_with_cli(&self, cli: &ListArgs) -> ListArgs {
        let base = self.to_list_args();
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
            assignee: cli.assignee.clone().or(base.assignee),
            priority_min: cli.priority_min.or(base.priority_min),
            priority_max: cli.priority_max.or(base.priority_max),
            title_contains: cli.title_contains.clone().or(base.title_contains),
            desc_contains: cli.desc_contains.clone().or(base.desc_contains),
            notes_contains: cli.notes_contains.clone().or(base.notes_contains),
            limit: cli.limit.or(base.limit),
            sort: cli.sort.clone().or(base.sort),
            // Bool fields: CLI true overrides saved
            unassigned: cli.unassigned || base.unassigned,
            all: cli.all || base.all,
            reverse: cli.reverse || base.reverse,
            deferred: cli.deferred || base.deferred,
            overdue: cli.overdue || base.overdue,
            // Output fields from CLI only
            long: cli.long,
            pretty: cli.pretty,
            format: cli.format,
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
pub fn execute(command: &QueryCommands, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    match command {
        QueryCommands::Save(args) => query_save(args, &mut storage_ctx.storage, json),
        QueryCommands::Run(args) => query_run(args, &storage_ctx.storage, json, cli, &beads_dir),
        QueryCommands::List => query_list(&storage_ctx.storage, json),
        QueryCommands::Delete(args) => query_delete(args, &mut storage_ctx.storage, json),
    }
}

fn query_save(
    args: &QuerySaveArgs,
    storage: &mut crate::storage::SqliteStorage,
    json: bool,
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
            format!("Query '{name}' already exists. Delete it first to replace."),
        ));
    }

    let saved_query = SavedQuery {
        name: name.to_string(),
        description: args.description.clone(),
        created_at: Utc::now(),
        filters: SavedFilters::from(&args.filters),
    };

    let value = serde_json::to_string(&saved_query)?;
    storage.set_config(&key, &value)?;

    info!(name, "Saved query created");

    if json {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "saved".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Saved query '{name}'");
    }

    Ok(())
}

fn query_run(
    args: &QueryRunArgs,
    storage: &crate::storage::SqliteStorage,
    json: bool,
    cli: &config::CliOverrides,
    _beads_dir: &Path,
) -> Result<()> {
    let name = args.name.trim();
    let key = format!("{QUERY_KEY_PREFIX}{name}");

    let value = storage
        .get_config(&key)?
        .ok_or_else(|| BeadsError::validation("query", format!("Query '{name}' not found")))?;

    let saved_query: SavedQuery = serde_json::from_str(&value).map_err(|e| {
        BeadsError::validation("saved_query", format!("Invalid saved query format: {e}"))
    })?;

    debug!(name, "Loaded saved query");

    // Merge saved filters with CLI overrides
    let merged_args = saved_query.filters.merge_with_cli(&args.filters);

    debug!(?merged_args, "Merged filters");

    // Execute list command with merged args
    // We call the list execute function directly
    super::list::execute(&merged_args, json, cli)
}

fn query_list(storage: &crate::storage::SqliteStorage, json: bool) -> Result<()> {
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

    if json {
        let output = QueryListOutput {
            count: queries.len(),
            queries,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if queries.is_empty() {
        println!("No saved queries");
    } else {
        println!("Saved queries:");
        for q in &queries {
            let desc = q.description.as_deref().unwrap_or("");
            if desc.is_empty() {
                println!("  {}", q.name);
            } else {
                println!("  {} - {}", q.name, desc);
            }
        }
        println!("\n{} query(ies) total", queries.len());
    }

    Ok(())
}

fn query_delete(
    args: &QueryDeleteArgs,
    storage: &mut crate::storage::SqliteStorage,
    json: bool,
) -> Result<()> {
    let name = args.name.trim();
    let key = format!("{QUERY_KEY_PREFIX}{name}");

    let deleted = storage.delete_config(&key)?;

    if !deleted {
        return Err(BeadsError::validation(
            "query",
            format!("Query '{name}' not found"),
        ));
    }

    info!(name, "Saved query deleted");

    if json {
        let output = QueryActionOutput {
            status: "ok".to_string(),
            name: name.to_string(),
            action: "deleted".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Deleted query '{name}'");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_saved_filters_from_list_args() {
        let args = ListArgs {
            status: vec!["open".to_string()],
            type_: vec!["bug".to_string()],
            assignee: Some("alice".to_string()),
            priority: vec!["1".to_string(), "2".to_string()],
            ..Default::default()
        };

        let filters = SavedFilters::from(&args);
        assert_eq!(filters.status, vec!["open"]);
        assert_eq!(filters.type_, vec!["bug"]);
        assert_eq!(filters.assignee, Some("alice".to_string()));
        assert_eq!(filters.priority, vec!["1", "2"]);
    }

    #[test]
    fn test_saved_filters_to_list_args() {
        let filters = SavedFilters {
            status: vec!["open".to_string()],
            assignee: Some("bob".to_string()),
            all: true,
            ..Default::default()
        };

        let args = filters.to_list_args();
        assert_eq!(args.status, vec!["open"]);
        assert_eq!(args.assignee, Some("bob".to_string()));
        assert!(args.all);
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
        assert_eq!(args.format, OutputFormat::default());
        assert!(args.fields.is_none());
    }

    #[test]
    fn test_from_list_args_ignores_output_fields() {
        let args = ListArgs {
            status: vec!["open".to_string()],
            long: true,
            pretty: true,
            format: OutputFormat::Json,
            fields: Some("id,title".to_string()),
            ..Default::default()
        };

        let filters = SavedFilters::from(&args);

        // Saved filters should not contain output-related fields
        // They're simply not part of the SavedFilters struct
        assert_eq!(filters.status, vec!["open"]);
    }
}
