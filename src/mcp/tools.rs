//! MCP tool handlers for the beads issue tracker.
//!
//! Seven tools — one per high-frequency agent workflow — following the
//! "≤ 7 tools per cluster" principle from the MCP design guide.
//!
//! Design philosophy: **Forgive by Default**.  Status/priority/type inputs
//! are auto-corrected ("wip" → `in_progress`, "urgent" → `critical`).
//! Placeholder IDs are detected and rejected with discovery hints.
//! Errors include `suggested_tool_calls` so agents know what to do next.

use std::sync::Arc;

use fastmcp_rust::{
    Content, McpContext, McpError, McpErrorCode, McpResult, Tool, ToolAnnotations, ToolHandler,
};
use serde_json::{Value, json};

use crate::error::{BeadsError, ErrorCode, StructuredError};
use crate::model::acceptance::{AcceptanceCriteriaOutput, AcceptanceEdit, plan_acceptance_edit};
use crate::model::{Comment, DependencyType, Issue, IssueType, Priority, Status};
use crate::storage::{IssueUpdate, ListFilters, SqliteStorage};
use crate::validation::{CommentValidator, IssueValidator, LabelValidator};

use super::{BeadsState, ensure_not_shutting_down, mcp_ready_issues};

// ---------------------------------------------------------------------------
// Constants — pre-computed sets for O(1) placeholder detection
// ---------------------------------------------------------------------------

/// Field keys in `update_issue` that map to `IssueUpdate` struct fields.
/// Used to distinguish field updates from label/comment side-effects.
const UPDATE_FIELD_KEYS: &[&str] = &[
    "title",
    "description",
    "status",
    "priority",
    "type",
    "assignee",
    "owner",
    "due_at",
    "defer_until",
    "estimated_minutes",
    "external_ref",
];

/// Known placeholder strings agents hallucinate instead of real IDs.
const PLACEHOLDER_EXACT: &[&str] = &[
    "your_id",
    "your-id",
    "yourid",
    "your_issue_id",
    "issue_id",
    "issue-id",
    "issueid",
    "example",
    "example_id",
    "example-id",
    "test",
    "test_id",
    "test-id",
    "foo",
    "bar",
    "baz",
    "qux",
    "xxx",
    "yyy",
    "zzz",
    "placeholder",
    "replace_me",
    "replace-me",
    "todo",
    "fixme",
    "tbd",
    "id",
    "issue",
    "some_id",
    "some-id",
    "abc",
    "abc123",
    "id_here",
    "insert_id",
    "none",
    "null",
    "undefined",
    "n/a",
];

const SHOW_ISSUE_BATCH_MAX: usize = 100;
const LIST_ISSUES_BATCH_MAX: usize = 100;
const CREATE_ISSUE_BATCH_MAX: usize = 100;
const UPDATE_ISSUE_BATCH_MAX: usize = 100;
const CLOSE_ISSUE_BATCH_MAX: usize = 100;
const MANAGE_DEPENDENCIES_BATCH_MAX: usize = 100;

const LIST_ISSUES_SINGLE_ARG_KEYS: &[&str] = &[
    "status",
    "type",
    "priority",
    "assignee",
    "labels",
    "title",
    "search",
    "include_closed",
    "limit",
    "sort",
];

// ---------------------------------------------------------------------------
// Placeholder detection
// ---------------------------------------------------------------------------

/// Detect if a string looks like a placeholder rather than a real issue ID.
/// Returns a structured error with discovery hints if detected.
fn detect_placeholder(s: &str) -> Option<McpError> {
    let lower = s.to_lowercase();

    // Exact match against known placeholders
    if PLACEHOLDER_EXACT.contains(&lower.as_str()) {
        return Some(placeholder_error(s));
    }

    // Pattern-based detection
    if lower.starts_with('$')
        || lower.starts_with('{')
        || lower.starts_with('<')
        || lower.starts_with('[')
    {
        return Some(placeholder_error(s));
    }

    // Substring detection — only match patterns specific to hallucinated IDs.
    // Broader terms like "replace" and "example" are already covered by exact
    // matches; substring matching them would reject legitimate IDs whose
    // prefix or hash portion happens to contain those words.
    if lower.contains("your_") || lower.contains("placeholder") {
        return Some(placeholder_error(s));
    }

    None
}

fn placeholder_error(s: &str) -> McpError {
    McpError::with_data(
        McpErrorCode::InvalidParams,
        format!("'{s}' looks like a placeholder, not a real issue ID"),
        json!({
            "error_type": "PLACEHOLDER_DETECTED",
            "provided": s,
            "recoverable": true,
            "hint": "Use list_issues to discover real issue IDs, or project_overview for a summary",
            "suggested_tool_calls": [
                {"tool": "list_issues", "arguments": {}},
                {"tool": "project_overview", "arguments": {}}
            ]
        }),
    )
}

fn tombstone_issue_err(id: &str) -> McpError {
    McpError::with_data(
        McpErrorCode::InvalidParams,
        format!("Issue '{id}' is tombstoned and cannot be used for this operation"),
        json!({
            "error_type": "TOMBSTONE_ISSUE",
            "id": id,
            "recoverable": false,
            "hint": "Use list_issues to choose an active issue, or create a new issue instead"
        }),
    )
}

/// Check for placeholder and validate that ID exists as an active issue.
/// Returns fuzzy suggestions with `suggested_tool_calls` if not found.
fn require_valid_issue(storage: &SqliteStorage, id: &str) -> McpResult<()> {
    // Check DB first: if the ID exists, it's real regardless of name.
    if let Some(issue) = storage.get_issue(id).map_err(beads_to_mcp)? {
        if issue.status == Status::Tombstone {
            return Err(tombstone_issue_err(id));
        }
        return Ok(());
    }
    // ID not found — give a more helpful placeholder error if the ID looks
    // hallucinated, otherwise give a standard not-found error.
    if let Some(err) = detect_placeholder(id) {
        return Err(err);
    }
    Err(issue_not_found_err(storage, id)?)
}

fn id_lookup_failed(operation: &str, err: &BeadsError) -> McpError {
    let structured = StructuredError::from_error(err);
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        format!("Failed to check issue ID existence during {operation}: {err}"),
        json!({
            "error_type": "ID_LOOKUP_FAILED",
            "operation": operation,
            "source_error_type": structured.code.as_str(),
            "message": structured.message,
            "recoverable": structured.retryable,
        }),
    )
}

fn no_available_child_id(parent_id: &str, start: u32, limit: u32) -> McpError {
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        format!("Could not find an available child ID for {parent_id}"),
        json!({
            "error_type": "NO_AVAILABLE_CHILD_ID",
            "parent_id": parent_id,
            "first_candidate_number": start,
            "last_candidate_number": limit,
            "recoverable": false,
        }),
    )
}

fn next_available_child_id<F>(parent_id: &str, start: u32, id_exists: F) -> McpResult<String>
where
    F: Fn(&str) -> Result<bool, BeadsError>,
{
    let limit = start.saturating_add(100);
    let mut num = start;
    loop {
        let candidate = crate::util::id::child_id(parent_id, num);
        if !id_exists(&candidate).map_err(|err| id_lookup_failed("child ID generation", &err))? {
            return Ok(candidate);
        }

        if num >= limit {
            return Err(no_available_child_id(parent_id, start, limit));
        }
        num = num
            .checked_add(1)
            .ok_or_else(|| no_available_child_id(parent_id, start, limit))?;
    }
}

fn generate_issue_id_with_checked_lookup<F>(
    title: &str,
    actor: &str,
    now: chrono::DateTime<chrono::Utc>,
    prefix: &str,
    issue_count: usize,
    id_exists: F,
) -> McpResult<String>
where
    F: Fn(&str) -> Result<bool, BeadsError>,
{
    let id_gen = crate::util::id::IdGenerator::new(
        crate::util::id::IdConfig::with_prefix(prefix).map_err(beads_to_mcp)?,
    );
    match id_gen.generate(title, None, Some(actor), now, issue_count, id_exists) {
        Ok(id) => Ok(id),
        Err(err @ BeadsError::IdCollision { .. }) => Err(beads_to_mcp(err)),
        Err(err) => Err(id_lookup_failed("hash ID generation", &err)),
    }
}

// ---------------------------------------------------------------------------
// Structured error builders
// ---------------------------------------------------------------------------

/// Convert a `BeadsError` into a structured `McpError` with machine-readable
/// data payload, recovery hints, and fuzzy-match suggestions.
fn beads_to_mcp(err: impl Into<crate::BeadsError>) -> McpError {
    let beads_err = err.into();
    let structured = StructuredError::from_error(&beads_err);

    let mcp_code = match structured.code {
        // Validation errors → invalid params
        ErrorCode::ValidationFailed
        | ErrorCode::InvalidStatus
        | ErrorCode::InvalidType
        | ErrorCode::InvalidPriority
        | ErrorCode::RequiredField
        | ErrorCode::InvalidId => McpErrorCode::InvalidParams,
        // Issue / dependency / operational errors → tool execution error
        ErrorCode::IssueNotFound
        | ErrorCode::AmbiguousId
        | ErrorCode::ShuttingDown
        | ErrorCode::NothingToDo
        | ErrorCode::CloseIncomplete
        | ErrorCode::CycleDetected
        | ErrorCode::DependencyNotFound
        | ErrorCode::HasDependents
        | ErrorCode::SelfDependency
        | ErrorCode::DuplicateDependency
        | ErrorCode::IdCollision => McpErrorCode::ToolExecutionError,
        // Database / config / IO → internal
        _ => McpErrorCode::InternalError,
    };

    let mut data = json!({
        "error_type": structured.code.as_str(),
        "recoverable": structured.retryable,
        "message": structured.message,
    });

    if let Some(hint) = &structured.hint {
        data["hint"] = json!(hint);
    }
    if let Some(ctx) = &structured.context
        && let Some(similar) = ctx.get("similar_ids")
    {
        data["suggestions"] = similar.clone();
    }

    // Add contextual suggested_tool_calls based on error type
    match structured.code {
        ErrorCode::IssueNotFound | ErrorCode::AmbiguousId => {
            data["suggested_tool_calls"] = json!([{"tool": "list_issues", "arguments": {}}]);
        }
        ErrorCode::CycleDetected => {
            // Suggest list_issues so the agent can discover IDs and
            // inspect the dependency graph.  We can't suggest tools
            // that require an `id` param because we don't know which
            // ID is relevant from the error alone.
            data["suggested_tool_calls"] = json!([
                {"tool": "list_issues", "arguments": {}}
            ]);
        }
        ErrorCode::InvalidStatus => {
            data["available_options"] = json!([
                "open",
                "in_progress",
                "blocked",
                "deferred",
                "draft",
                "closed",
                "pinned"
            ]);
            data["fix_hint"] = json!("Aliases also accepted: wip, todo, done, stuck, later, hold");
        }
        ErrorCode::InvalidPriority => {
            data["available_options"] = json!(["critical", "high", "medium", "low", "backlog"]);
            data["fix_hint"] =
                json!("Aliases also accepted: urgent, important, normal, minor, someday");
        }
        ErrorCode::ShuttingDown => {
            if let Some(object) = data.as_object_mut() {
                object.insert(
                    "recovery".to_string(),
                    json!("Retry after starting a fresh br serve process"),
                );
            }
        }
        _ => {
            data["discovery_hint"] =
                json!("Use list_issues tool or beads://labels resource to find valid values");
        }
    }

    McpError::with_data(mcp_code, structured.message, data)
}

/// Build a structured "issue not found" `McpError` with fuzzy ID suggestions
/// and `suggested_tool_calls` pointing to the best next action.
fn issue_not_found_err(storage: &SqliteStorage, id: &str) -> McpResult<McpError> {
    let all_ids = storage.get_all_ids().map_err(beads_to_mcp)?;
    let structured = StructuredError::issue_not_found(id, &all_ids);

    let mut data = json!({
        "error_type": "ISSUE_NOT_FOUND",
        "recoverable": true,
        "message": structured.message,
        "discovery_hint": "Use list_issues to find valid issue IDs",
    });

    if let Some(hint) = &structured.hint {
        data["hint"] = json!(hint);
    }

    // Build suggested_tool_calls based on whether we have fuzzy matches
    let mut suggested_calls = Vec::new();
    if let Some(ctx) = &structured.context
        && let Some(similar) = ctx.get("similar_ids")
    {
        data["suggestions"] = similar.clone();
        // If exactly one match, suggest show_issue directly
        if let Some(arr) = similar.as_array()
            && arr.len() == 1
            && let Some(suggested_id) = arr[0].as_str()
        {
            suggested_calls.push(json!({
                "tool": "show_issue",
                "arguments": {"id": suggested_id}
            }));
        }
    }
    suggested_calls.push(json!({"tool": "list_issues", "arguments": {}}));
    data["suggested_tool_calls"] = json!(suggested_calls);

    Ok(McpError::with_data(
        McpErrorCode::ToolExecutionError,
        structured.message,
        data,
    ))
}

fn storage_read_warning(operation: &str, err: &crate::BeadsError) -> serde_json::Value {
    let structured = StructuredError::from_error(err);
    let mut warning = json!({
        "warning_type": "STORAGE_READ_FAILED",
        "operation": operation,
        "error_type": structured.code.as_str(),
        "message": structured.message,
        "recoverable": structured.retryable,
    });

    if let Some(hint) = structured.hint {
        warning["hint"] = json!(hint);
    }
    if let Some(context) = structured.context {
        warning["context"] = context;
    }

    warning
}

fn open(state: &BeadsState) -> McpResult<SqliteStorage> {
    state.open_read_storage().map_err(beads_to_mcp)
}

fn cached_read_json<F>(state: &BeadsState, key: String, build: F) -> McpResult<Value>
where
    F: FnOnce(&SqliteStorage) -> McpResult<Value>,
{
    if let Some(value) = state.cached_read_json(&key) {
        return Ok(value);
    }

    let before = state.capture_read_snapshot_witness();
    let storage = open(state)?;
    let value = build(&storage)?;
    state.store_read_json_snapshot(key, before, &value);

    Ok(value)
}

// ---------------------------------------------------------------------------
// Input coercion helpers — Forgive by Default
// ---------------------------------------------------------------------------

/// Parse a status string with coercion. Returns the status and an optional
/// warning if the input was auto-corrected (e.g. "wip" → "in_progress").
fn parse_status(s: &str) -> McpResult<(Status, Option<String>)> {
    let lower = s.to_lowercase();
    let (status, canonical) = match lower.as_str() {
        "open" | "new" | "todo" => (Status::Open, "open"),
        "in_progress" | "in-progress" | "inprogress" | "wip" | "working" | "active" | "started" => {
            (Status::InProgress, "in_progress")
        }
        "blocked" | "stuck" | "waiting" => (Status::Blocked, "blocked"),
        "deferred" | "later" | "postponed" | "backlogged" => (Status::Deferred, "deferred"),
        "draft" => (Status::Draft, "draft"),
        "closed" | "done" | "completed" | "resolved" | "fixed" | "wontfix" | "cancelled" => {
            (Status::Closed, "closed")
        }
        "pinned" | "sticky" | "hold" | "on_hold" | "on-hold" => (Status::Pinned, "pinned"),
        _ => {
            return Err(McpError::with_data(
                McpErrorCode::InvalidParams,
                format!("Unknown status '{s}'"),
                json!({
                    "error_type": "INVALID_STATUS",
                    "provided": s,
                    "recoverable": true,
                    "available_options": ["open", "in_progress", "blocked", "deferred", "draft", "closed", "pinned"],
                    "aliases": {
                        "open": ["new", "todo"],
                        "in_progress": ["wip", "working", "active", "started"],
                        "blocked": ["stuck", "waiting"],
                        "deferred": ["later", "postponed"],
                        "closed": ["done", "completed", "resolved", "fixed"],
                        "pinned": ["hold", "sticky", "on_hold"]
                    },
                    "fix_hint": "Use one of the available options or their aliases"
                }),
            ));
        }
    };

    let warning = (lower != canonical).then(|| format!("'{s}' interpreted as '{canonical}'"));

    Ok((status, warning))
}

/// Parse a priority string with coercion.
fn parse_priority(s: &str) -> McpResult<(Priority, Option<String>)> {
    let lower = s.to_lowercase();
    let (priority, canonical) = match lower.as_str() {
        "critical" | "p0" | "0" | "urgent" | "asap" | "emergency" => {
            (Priority::CRITICAL, "critical")
        }
        "high" | "p1" | "1" | "important" => (Priority::HIGH, "high"),
        "medium" | "p2" | "2" | "normal" | "default" | "mid" => (Priority::MEDIUM, "medium"),
        "low" | "p3" | "3" | "minor" | "trivial" | "nice_to_have" | "nice-to-have" => {
            (Priority::LOW, "low")
        }
        "backlog" | "p4" | "4" | "someday" | "eventually" | "whenever" => {
            (Priority::BACKLOG, "backlog")
        }
        _ => {
            return Err(McpError::with_data(
                McpErrorCode::InvalidParams,
                format!("Unknown priority '{s}'"),
                json!({
                    "error_type": "INVALID_PRIORITY",
                    "provided": s,
                    "recoverable": true,
                    "available_options": ["critical", "high", "medium", "low", "backlog"],
                    "aliases": {
                        "critical": ["p0", "urgent", "asap", "emergency"],
                        "high": ["p1", "important"],
                        "medium": ["p2", "normal", "default"],
                        "low": ["p3", "minor", "trivial"],
                        "backlog": ["p4", "someday", "eventually"]
                    },
                    "fix_hint": "Use one of the available options or their aliases"
                }),
            ));
        }
    };

    // Canonical names and numeric aliases don't need a coercion warning
    let warning = (lower != canonical && !lower.starts_with('p') && lower.parse::<u8>().is_err())
        .then(|| format!("'{s}' interpreted as '{canonical}'"));

    Ok((priority, warning))
}

/// Parse an issue type string with coercion.
fn parse_issue_type(s: &str) -> (IssueType, Option<String>) {
    let lower = s.to_lowercase();
    let (issue_type, canonical) = match lower.as_str() {
        "task" | "issue" => (IssueType::Task, "task"),
        "bug" | "bugfix" | "defect" | "regression" => (IssueType::Bug, "bug"),
        "feature" | "feat" | "enhancement" | "story" | "request" => (IssueType::Feature, "feature"),
        "epic" => (IssueType::Epic, "epic"),
        "chore" | "maintenance" | "cleanup" | "refactor" | "tech_debt" | "tech-debt" => {
            (IssueType::Chore, "chore")
        }
        "docs" | "documentation" | "doc" => (IssueType::Docs, "docs"),
        "question" | "q" | "help" => (IssueType::Question, "question"),
        other => return (IssueType::Custom(other.to_string()), None),
    };

    let warning = (lower != canonical).then(|| format!("'{s}' interpreted as '{canonical}'"));

    (issue_type, warning)
}

/// Validate and coerce a dependency type string. Dependency types use
/// kebab-case internally ("parent-child", "waits-for") but agents often
/// pass underscores or abbreviated forms.
fn parse_dep_type(s: &str) -> McpResult<(String, Option<String>)> {
    let lower = s.to_lowercase();
    let (canonical, alias) = match lower.as_str() {
        "blocks" | "block" | "blocking" => ("blocks", true),
        "related" => ("related", false),
        "parent-child" | "parent_child" | "parentchild" | "parent" | "child" => {
            ("parent-child", true)
        }
        "waits-for" | "waits_for" | "waitfor" | "waitsfor" | "waiting" => ("waits-for", true),
        "duplicates" | "duplicate" | "dupe" | "dup" => ("duplicates", true),
        "supersedes" | "supersede" | "replaces" => ("supersedes", true),
        "caused-by" | "caused_by" | "causedby" | "root_cause" | "root-cause" => ("caused-by", true),
        "conditional-blocks" | "conditional_blocks" | "conditionalblocks" => {
            ("conditional-blocks", true)
        }
        "discovered-from" | "discovered_from" | "discoveredfrom" => ("discovered-from", true),
        "replies-to" | "replies_to" | "repliesto" | "reply" => ("replies-to", true),
        "relates-to" | "relates_to" | "relatesto" => ("relates-to", true),
        _ => {
            return Err(McpError::with_data(
                McpErrorCode::InvalidParams,
                format!("Unknown dependency type '{s}'"),
                json!({
                    "error_type": "INVALID_DEP_TYPE",
                    "provided": s,
                    "recoverable": true,
                    "available_options": [
                        "blocks", "related", "parent-child", "waits-for",
                        "duplicates", "supersedes", "caused-by",
                        "conditional-blocks", "discovered-from", "replies-to", "relates-to"
                    ],
                    "fix_hint": "Dependency types use kebab-case (e.g., 'parent-child', not 'parent_child')"
                }),
            ));
        }
    };

    let warning =
        (alias && lower != canonical).then(|| format!("'{s}' interpreted as '{canonical}'"));

    Ok((canonical.to_string(), warning))
}

/// Parse an ISO 8601 timestamp with coercion (Z → +00:00, slashes → dashes).
fn parse_timestamp(s: &str) -> McpResult<(chrono::DateTime<chrono::Utc>, Option<String>)> {
    let mut normalized = s.trim().to_string();
    let mut coerced = false;

    // Slashes → dashes
    if normalized.contains('/') {
        normalized = normalized.replace('/', "-");
        coerced = true;
    }

    // Try parsing as-is first (handles Z suffix, full ISO 8601)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        let warning = coerced.then(|| format!("'{s}' normalized to '{normalized}'"));
        return Ok((dt.with_timezone(&chrono::Utc), warning));
    }

    // Try with appended Z for bare timestamps (no timezone indicator).
    // We check for timezone patterns near the end, not substring matches,
    // because "-0" appears in date portions (e.g. months 01-09).
    let has_tz_suffix = normalized.contains('Z')
        || normalized.contains('+')
        || normalized.ends_with('z')
        || normalized
            .rfind('-')
            .is_some_and(|pos| pos > 10 && normalized[pos..].contains(':'));
    if !has_tz_suffix {
        let with_tz = format!("{normalized}Z");
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&with_tz) {
            return Ok((
                dt.with_timezone(&chrono::Utc),
                Some(format!("'{s}' interpreted as UTC (missing timezone)")),
            ));
        }
    }

    // Try parsing date-only (YYYY-MM-DD → start of day UTC)
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        let Some(midnight) = date.and_hms_opt(0, 0, 0) else {
            return Err(McpError::invalid_params(format!(
                "Cannot normalize date '{s}' to midnight UTC"
            )));
        };
        let dt = midnight.and_utc();
        return Ok((
            dt,
            Some(format!("'{s}' interpreted as '{}'", dt.to_rfc3339())),
        ));
    }

    Err(McpError::with_data(
        McpErrorCode::InvalidParams,
        format!("Cannot parse timestamp '{s}'"),
        json!({
            "error_type": "INVALID_TIMESTAMP",
            "provided": s,
            "recoverable": true,
            "expected_format": "ISO 8601 (e.g., '2026-03-15T10:00:00Z' or '2026-03-15')",
            "common_mistakes": [
                "Missing timezone — add Z or +00:00",
                "Using slashes — use dashes (2026-03-15 not 2026/03/15)",
                "Date-only works: '2026-03-15' → start of day UTC"
            ]
        }),
    ))
}

/// Sanitize a search query for FTS5 compatibility.
/// Returns `None` for bare wildcards (meaning "return all").
fn sanitize_search(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip leading wildcards (FTS5 doesn't support *foo)
    let cleaned = trimmed.trim_start_matches('*');
    // Bare wildcard or dot → return all
    if cleaned.is_empty() || cleaned == "." {
        return None;
    }
    Some(cleaned.to_string())
}

/// Read a nullable string field from JSON args: present-string → `Some(Some(s))`,
/// present-null → `Some(None)`, absent → `None`.
/// Returns an error if the value is present but neither a string nor null.
#[allow(clippy::option_option)]
fn nullable_str(args: &serde_json::Value, key: &str) -> McpResult<Option<Option<String>>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(v) => v.as_str().map_or_else(
            || {
                Err(McpError::invalid_params(format!(
                    "'{key}' must be a string or null, got {v}"
                )))
            },
            |s| Ok(Some(Some(s.to_string()))),
        ),
    }
}

fn required_str_arg(args: &serde_json::Value, key: &str) -> McpResult<String> {
    match args.get(key) {
        None => Err(McpError::invalid_params(format!("'{key}' is required"))),
        Some(v) => v.as_str().map_or_else(
            || {
                Err(McpError::invalid_params(format!(
                    "'{key}' must be a string, got {v}"
                )))
            },
            |s| Ok(s.to_string()),
        ),
    }
}

fn optional_str_arg(args: &serde_json::Value, key: &str) -> McpResult<Option<String>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v.as_str().map_or_else(
            || {
                Err(McpError::invalid_params(format!(
                    "'{key}' must be a string, got {v}"
                )))
            },
            |s| Ok(Some(s.to_string())),
        ),
    }
}

fn optional_bool_arg(args: &serde_json::Value, key: &str) -> McpResult<Option<bool>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v.as_bool().map_or_else(
            || {
                Err(McpError::invalid_params(format!(
                    "'{key}' must be a boolean, got {v}"
                )))
            },
            |b| Ok(Some(b)),
        ),
    }
}

fn optional_u64_arg(args: &serde_json::Value, key: &str) -> McpResult<Option<u64>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v.as_u64().map_or_else(
            || {
                Err(McpError::invalid_params(format!(
                    "'{key}' must be a non-negative integer, got {v}"
                )))
            },
            |n| Ok(Some(n)),
        ),
    }
}

fn optional_string_array_arg(args: &serde_json::Value, key: &str) -> McpResult<Vec<String>> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };

    let array = value.as_array().ok_or_else(|| {
        McpError::invalid_params(format!("'{key}' must be an array of strings, got {value}"))
    })?;

    array
        .iter()
        .enumerate()
        .map(|(idx, value)| {
            value.as_str().map_or_else(
                || {
                    Err(McpError::invalid_params(format!(
                        "'{key}[{idx}]' must be a string, got {value}"
                    )))
                },
                |s| Ok(s.to_string()),
            )
        })
        .collect()
}

fn optional_non_empty_string_array_arg(
    args: &serde_json::Value,
    key: &str,
    max_len: usize,
) -> McpResult<Option<Vec<String>>> {
    if args.get(key).is_none() {
        return Ok(None);
    }

    let values = optional_string_array_arg(args, key)?;
    if values.is_empty() {
        return Err(McpError::invalid_params(format!(
            "'{key}' must include at least one issue ID"
        )));
    }
    if values.len() > max_len {
        return Err(McpError::invalid_params(format!(
            "'{key}' supports at most {max_len} issue IDs per call, got {}",
            values.len()
        )));
    }

    Ok(Some(values))
}

fn optional_label_array_arg(args: &serde_json::Value, key: &str) -> McpResult<Vec<String>> {
    let labels = optional_string_array_arg(args, key)?;
    for (idx, label) in labels.iter().enumerate() {
        LabelValidator::validate(label).map_err(|err| {
            McpError::invalid_params(format!("'{key}[{idx}]' invalid label: {}", err.message))
        })?;
    }
    Ok(labels)
}

fn validate_mcp_title(title: &str) -> McpResult<()> {
    if title.trim().is_empty() || title.chars().count() > 500 {
        return Err(McpError::invalid_params("Title must be 1-500 characters"));
    }
    Ok(())
}

fn validate_mcp_comment(issue_id: &str, author: &str, body: &str) -> McpResult<()> {
    let comment = Comment {
        id: 1,
        issue_id: issue_id.to_string(),
        author: author.to_string(),
        body: body.to_string(),
        created_at: chrono::Utc::now(),
    };

    CommentValidator::validate(&comment)
        .map_err(BeadsError::from_validation_errors)
        .map_err(beads_to_mcp)
}

/// Parse update fields from JSON args into an `IssueUpdate` + coercion warnings.
/// Extracted to keep `UpdateIssueTool::call` under the line limit.
#[allow(clippy::too_many_lines)]
fn parse_update_fields(
    id: &str,
    args: &serde_json::Value,
) -> McpResult<(IssueUpdate, Vec<String>)> {
    let mut coercions: Vec<String> = Vec::new();
    let mut updates = IssueUpdate::default();

    if let Some(title) = optional_str_arg(args, "title")? {
        validate_mcp_title(&title)?;
        updates.title = Some(title);
    }
    updates.description = nullable_str(args, "description")?;
    if let Some(s) = optional_str_arg(args, "status")? {
        let (status, warning) = parse_status(&s)?;

        // Intercept status→closed: redirect to close_issue
        if status == Status::Closed {
            return Err(McpError::with_data(
                McpErrorCode::ToolExecutionError,
                "To close an issue, use close_issue which properly records close metadata",
                json!({
                    "error_type": "USE_CLOSE_ISSUE",
                    "recoverable": true,
                    "hint": "close_issue records close_reason and closed_at timestamp for proper audit trail",
                    "suggested_tool_calls": [{
                        "tool": "close_issue",
                        "arguments": {"id": id}
                    }]
                }),
            ));
        }

        if let Some(w) = warning {
            coercions.push(w);
        }
        updates.status = Some(status);
    }
    if let Some(p) = optional_str_arg(args, "priority")? {
        let (priority, warning) = parse_priority(&p)?;
        if let Some(w) = warning {
            coercions.push(w);
        }
        updates.priority = Some(priority);
    }
    if let Some(t) = optional_str_arg(args, "type")? {
        let (issue_type, warning) = parse_issue_type(&t);
        if let Some(w) = warning {
            coercions.push(w);
        }
        updates.issue_type = Some(issue_type);
    }
    updates.assignee = nullable_str(args, "assignee")?;
    updates.owner = nullable_str(args, "owner")?;
    if let Some(v) = args.get("due_at") {
        if v.is_null() {
            updates.due_at = Some(None);
        } else if let Some(s) = v.as_str() {
            let (dt, warning) = parse_timestamp(s)?;
            if let Some(w) = warning {
                coercions.push(format!("due_at: {w}"));
            }
            updates.due_at = Some(Some(dt));
        } else {
            return Err(McpError::invalid_params(format!(
                "'due_at' must be an ISO 8601 string or null, got {v}"
            )));
        }
    }
    if let Some(v) = args.get("defer_until") {
        if v.is_null() {
            updates.defer_until = Some(None);
        } else if let Some(s) = v.as_str() {
            let (dt, warning) = parse_timestamp(s)?;
            if let Some(w) = warning {
                coercions.push(format!("defer_until: {w}"));
            }
            updates.defer_until = Some(Some(dt));
        } else {
            return Err(McpError::invalid_params(format!(
                "'defer_until' must be an ISO 8601 string or null, got {v}"
            )));
        }
    }
    if let Some(v) = args.get("estimated_minutes") {
        if v.is_null() {
            updates.estimated_minutes = Some(None);
        } else if let Some(n) = v.as_i64() {
            let mins = i32::try_from(n)
                .map_err(|_| McpError::invalid_params("estimated_minutes must fit in i32"))?;
            updates.estimated_minutes = Some(Some(mins));
        } else if let Some(s) = v.as_str() {
            // Forgive by Default: coerce string → integer
            let mins: i32 = s.parse().map_err(|_| {
                McpError::invalid_params(format!(
                    "'estimated_minutes' must be an integer, got string '{s}'"
                ))
            })?;
            coercions.push(format!(
                "estimated_minutes: string '{s}' coerced to integer {mins}"
            ));
            updates.estimated_minutes = Some(Some(mins));
        } else {
            return Err(McpError::invalid_params(format!(
                "'estimated_minutes' must be an integer or null, got {v}"
            )));
        }
    }
    updates.external_ref = nullable_str(args, "external_ref")?;

    Ok((updates, coercions))
}

/// Serialize an issue to JSON for output.
fn issue_json(issue: &Issue) -> serde_json::Value {
    serde_json::to_value(issue).unwrap_or_else(|_| json!({"id": issue.id, "title": issue.title}))
}

// ---------------------------------------------------------------------------
// 1. list_issues
// ---------------------------------------------------------------------------

/// Build `ListFilters` from the JSON arguments, collecting coercion warnings.
fn build_list_filters(
    args: &serde_json::Value,
    coercions: &mut Vec<String>,
) -> McpResult<ListFilters> {
    let statuses = optional_str_arg(args, "status")?
        .map(|s| {
            s.split(',')
                .map(|p| {
                    let (status, warning) = parse_status(p.trim())?;
                    if let Some(w) = warning {
                        coercions.push(w);
                    }
                    Ok(status)
                })
                .collect::<McpResult<Vec<_>>>()
        })
        .transpose()?;

    let types = optional_str_arg(args, "type")?.map(|s| {
        s.split(',')
            .map(|p| {
                let (t, warning) = parse_issue_type(p.trim());
                if let Some(w) = warning {
                    coercions.push(w);
                }
                t
            })
            .collect::<Vec<_>>()
    });

    let priorities = optional_str_arg(args, "priority")?
        .map(|s| {
            s.split(',')
                .map(|p| {
                    let (prio, warning) = parse_priority(p.trim())?;
                    if let Some(w) = warning {
                        coercions.push(w);
                    }
                    Ok(prio)
                })
                .collect::<McpResult<Vec<_>>>()
        })
        .transpose()?;

    let labels = optional_str_arg(args, "labels")?.map(|s| {
        s.split(',')
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
    });

    let include_closed = optional_bool_arg(args, "include_closed")?.unwrap_or(false);

    let raw_limit = optional_u64_arg(args, "limit")?.unwrap_or(50);
    let limit = Some(raw_limit.min(500) as usize);

    let sort = optional_str_arg(args, "sort")?;

    let title_contains = optional_str_arg(args, "title")?;

    // Forgive by Default: if the status filter explicitly includes Closed or
    // Deferred, automatically enable the corresponding include flag so the
    // query doesn't contradict itself (the default exclusion filters would
    // otherwise produce zero results).
    let include_closed = include_closed
        || statuses
            .as_ref()
            .is_some_and(|s| s.contains(&Status::Closed));
    let include_deferred = statuses
        .as_ref()
        .is_some_and(|s| s.contains(&Status::Deferred));

    Ok(ListFilters {
        statuses,
        types,
        priorities,
        assignee: optional_str_arg(args, "assignee")?,
        include_closed,
        include_deferred,
        limit,
        sort,
        labels,
        title_contains,
        ..ListFilters::default()
    })
}

fn list_issues_json(storage: &SqliteStorage, args: &Value) -> McpResult<Value> {
    let mut coercions: Vec<String> = Vec::new();
    let filters = build_list_filters(args, &mut coercions)?;

    let search_query = optional_str_arg(args, "search")?;

    let issues = if let Some(raw_q) = search_query.as_deref() {
        if let Some(q) = sanitize_search(raw_q) {
            storage.search_issues(&q, &filters).map_err(beads_to_mcp)?
        } else {
            // Bare wildcard — fall back to list (no search filter)
            coercions.push(format!(
                "search '{raw_q}' was a bare wildcard, returning all"
            ));
            storage.list_issues(&filters).map_err(beads_to_mcp)?
        }
    } else {
        storage.list_issues(&filters).map_err(beads_to_mcp)?
    };

    let mut result = json!({
        "count": issues.len(),
        "issues": issues.iter().map(issue_json).collect::<Vec<_>>(),
    });

    // Contextual next_actions
    if issues.is_empty() {
        result["next_actions"] = json!([
            "No issues matched. Try broadening filters or removing some.",
            "Use project_overview to see what's in the tracker"
        ]);
    } else {
        result["next_actions"] = json!([
            "Use show_issue with an issue ID for full details",
            "Narrow results with additional filters"
        ]);
    }

    if !coercions.is_empty() {
        result["coercions"] = json!(coercions);
    }

    Ok(result)
}

fn list_issues_batch_items(args: &Value) -> McpResult<Option<Vec<Value>>> {
    let Some(value) = args.get("queries") else {
        return Ok(None);
    };

    if LIST_ISSUES_SINGLE_ARG_KEYS
        .iter()
        .any(|key| args.get(*key).is_some())
    {
        return Err(McpError::invalid_params(
            "Provide either list filters for a single query or queries[] for a batch, not both",
        ));
    }

    let array = value.as_array().ok_or_else(|| {
        McpError::invalid_params(format!(
            "'queries' must be an array of filter objects, got {value}"
        ))
    })?;
    if array.is_empty() {
        return Err(McpError::invalid_params(
            "'queries' must contain at least one filter object",
        ));
    }
    if array.len() > LIST_ISSUES_BATCH_MAX {
        return Err(McpError::invalid_params(format!(
            "'queries' accepts at most {LIST_ISSUES_BATCH_MAX} filter objects"
        )));
    }

    Ok(Some(array.clone()))
}

fn list_issues_batch_error_item(index: usize, args: &Value, err: McpError) -> Value {
    json!({
        "index": index,
        "query": args,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn list_issues_batch_json(storage: &SqliteStorage, queries: &[Value]) -> Value {
    let mut ok_count = 0_u64;
    let items: Vec<Value> = queries
        .iter()
        .enumerate()
        .map(|(index, query)| {
            if !query.is_object() {
                return list_issues_batch_error_item(
                    index,
                    query,
                    McpError::invalid_params(format!(
                        "'queries[{index}]' must be a filter object, got {query}"
                    )),
                );
            }

            match list_issues_json(storage, query) {
                Ok(result) => {
                    ok_count += 1;
                    json!({
                        "index": index,
                        "query": query,
                        "ok": true,
                        "result": result,
                    })
                }
                Err(err) => list_issues_batch_error_item(index, query, err),
            }
        })
        .collect();
    let count = queries.len() as u64;

    json!({
        "items": items,
        "count": count,
        "ok_count": ok_count,
        "error_count": count - ok_count,
    })
}

pub struct ListIssuesTool(Arc<BeadsState>);
impl ListIssuesTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ToolHandler for ListIssuesTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "list_issues".into(),
            description: Some(
                "List issues matching filters, or run multiple filtered list queries in one batch. Returns JSON issue summaries.\n\n\
                 Discovery: Use beads://schema for accepted status/type/priority values and beads://labels for valid label values.\n\
                 When to use: Exploring the backlog, finding issues by status/type/priority/label.\n\
                 NOT for: Getting full details on one issue — use show_issue instead.\n\
                 Do: Start broad (no filters) then narrow down. Use limit to cap output, or pass queries[] for a per-item batch envelope.\n\
                 Don't: Fetch all issues when you only need a count — use project_overview.\n\
                 Note: 'search' (full-text) and 'title' (substring) can be combined but 'search' is preferred for discovery.\n\
                 Inputs auto-corrected: 'wip' → in_progress, 'urgent' → critical, etc.\n\
                 Batch semantics: queries[] uses one read storage open and returns {items,count,ok_count,error_count}; each item has ok:true with the legacy result or ok:false with a structured error.\n\
                 Idempotency: Safe to retry; read-only."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status (comma-separated). Values: open, in_progress, blocked, deferred, draft, closed, pinned. Aliases accepted: wip, todo, done, stuck, later, hold."
                    },
                    "type": {
                        "type": "string",
                        "description": "Filter by issue type: task, bug, feature, epic, chore, docs, question. Aliases: feat, defect, enhancement, refactor, doc."
                    },
                    "priority": {
                        "type": "string",
                        "description": "Filter by priority: critical, high, medium, low, backlog. Aliases: urgent, important, normal, minor, someday."
                    },
                    "assignee": {
                        "type": "string",
                        "description": "Filter by assignee name"
                    },
                    "labels": {
                        "type": "string",
                        "description": "Filter by labels (comma-separated, AND logic). See beads://labels for valid values."
                    },
                    "title": {
                        "type": "string",
                        "description": "Filter by title substring (case-insensitive). For full-text search use 'search' instead."
                    },
                    "search": {
                        "type": "string",
                        "description": "Full-text search query against title/description. Leading wildcards are stripped."
                    },
                    "include_closed": {
                        "type": "boolean",
                        "description": "Include closed issues (default false)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max issues to return (default 50, max 500)"
                    },
                    "sort": {
                        "type": "string",
                        "description": "Sort field: priority, created, updated, title (default: updated)"
                    },
                    "queries": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": LIST_ISSUES_BATCH_MAX,
                        "description": "Batch of list filter objects using the same fields as a single list_issues call. Returns {items,count,ok_count,error_count}; each item is either {index,query,ok:true,result} using the legacy single-list result shape, or {index,query,ok:false,error}. Partial failures do not fail the whole batch.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "status": {"type": "string"},
                                "type": {"type": "string"},
                                "priority": {"type": "string"},
                                "assignee": {"type": "string"},
                                "labels": {"type": "string"},
                                "title": {"type": "string"},
                                "search": {"type": "string"},
                                "include_closed": {"type": "boolean"},
                                "limit": {"type": "integer"},
                                "sort": {"type": "string"}
                            }
                        }
                    }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: Some(true),
                destructive: Some(false),
                idempotent: Some(true),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        if let Some(queries) = list_issues_batch_items(&args)? {
            let key = format!("tool:list_issues_batch:{}", json!(&queries));
            let result = cached_read_json(&self.0, key, |storage| {
                Ok(list_issues_batch_json(storage, &queries))
            })?;
            return Ok(vec![Content::text(result.to_string())]);
        }

        let key = format!("tool:list_issues:{}", args);
        let result = cached_read_json(&self.0, key, |storage| list_issues_json(storage, &args))?;
        Ok(vec![Content::text(result.to_string())])
    }
}

// ---------------------------------------------------------------------------
// 2. show_issue
// ---------------------------------------------------------------------------

pub struct ShowIssueTool(Arc<BeadsState>);
impl ShowIssueTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn show_issue_json(storage: &SqliteStorage, id: &str) -> McpResult<Value> {
    let maybe_details = storage
        .get_issue_details(id, true, true, 20)
        .map_err(beads_to_mcp)?;
    let Some(details) = maybe_details else {
        return Err(issue_not_found_err(storage, id)?);
    };

    let mut result = serde_json::to_value(&details.issue).unwrap_or_default();
    if let Some(obj) = result.as_object_mut() {
        obj.insert("labels".into(), json!(details.labels));
        if !details.acceptance_items.is_empty() {
            obj.insert("acceptance_items".into(), json!(details.acceptance_items));
        }
        obj.insert("comments".into(), json!(details.comments));
        obj.insert(
            "dependencies".into(),
            json!(
                details
                    .dependencies
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id,
                            "title": d.title,
                            "status": d.status,
                            "dep_type": d.dep_type
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );
        obj.insert(
            "dependents".into(),
            json!(
                details
                    .dependents
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id,
                            "title": d.title,
                            "status": d.status,
                            "dep_type": d.dep_type
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );
        if let Some(parent) = &details.parent {
            obj.insert("parent".into(), json!(parent));
        }
        obj.insert(
            "recent_events".into(),
            json!(
                details
                    .events
                    .iter()
                    .take(10)
                    .map(|e| {
                        json!({
                            "type": e.event_type,
                            "actor": e.actor,
                            "old_value": e.old_value,
                            "new_value": e.new_value,
                            "created_at": e.created_at,
                            // Tier 1 attribution (issue #312, Layer 3
                            // capture-only); null when not supplied. Recorded
                            // only — never gated on.
                            "agent_name": e.agent_name,
                            "harness": e.harness,
                            "model": e.model
                        })
                    })
                    .collect::<Vec<_>>()
            ),
        );

        // Contextual next_actions based on issue state
        let mut actions: Vec<String> = Vec::new();
        if details.issue.status == Status::Blocked {
            actions.push(
                "This issue is blocked. Use manage_dependencies action 'list' to see blockers."
                    .into(),
            );
        }
        if details.issue.assignee.is_none() && details.issue.status != Status::Closed {
            actions.push("No assignee — consider assigning with update_issue.".into());
        }
        if details.issue.status == Status::Closed {
            actions.push("This issue is closed. Use list_issues to find open work.".into());
        } else {
            actions.push("Use update_issue to modify fields or add a comment.".into());
            actions.push("Use manage_dependencies to link to other issues.".into());
        }
        obj.insert("next_actions".into(), json!(actions));
    }

    Ok(result)
}

fn show_issue_batch_error_item(id: &str, err: McpError) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn mcp_error_json(err: McpError) -> Value {
    json!({
        "code": i32::from(err.code),
        "kind": format!("{:?}", err.code),
        "message": err.message,
        "data": err.data.unwrap_or(Value::Null),
    })
}

fn show_issue_batch_json(storage: &SqliteStorage, ids: &[String]) -> Value {
    let mut ok_count = 0_u64;
    let items: Vec<Value> = ids
        .iter()
        .map(|id| {
            if let Some(err) = detect_placeholder(id) {
                return show_issue_batch_error_item(id, err);
            }

            match show_issue_json(storage, id) {
                Ok(issue) => {
                    ok_count += 1;
                    json!({
                        "id": id,
                        "ok": true,
                        "issue": issue,
                    })
                }
                Err(err) => show_issue_batch_error_item(id, err),
            }
        })
        .collect();
    let count = u64::try_from(ids.len()).unwrap_or(u64::MAX);

    json!({
        "items": items,
        "count": count,
        "ok_count": ok_count,
        "error_count": count.saturating_sub(ok_count),
    })
}

fn show_issue_batch_ids(args: &Value) -> McpResult<Option<Vec<String>>> {
    if args.get("id").is_some() && args.get("ids").is_some() {
        return Err(McpError::invalid_params(
            "Provide either 'id' for a single issue or 'ids' for a batch, not both",
        ));
    }

    optional_non_empty_string_array_arg(args, "ids", SHOW_ISSUE_BATCH_MAX)
}

impl ToolHandler for ShowIssueTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "show_issue".into(),
            description: Some(
                "Get full details for one issue, or batch details for multiple known issue IDs.\n\n\
                 Discovery: Get IDs from list_issues or project_overview.\n\
                 When to use: You have one or more issue IDs and need complete context.\n\
                 NOT for: Browsing unknown issues — use list_issues instead.\n\
                 Do: Pass id for the legacy single-issue response, or ids[] for a per-item batch envelope.\n\
                 Don't: Guess IDs — use list_issues to discover them first.\n\
                 Common mistakes: Using placeholder IDs ('YOUR_ID') — these are detected.\n\
                 Batch semantics: ids[] uses one read storage open and returns per-item ok/error results.\n\
                 Idempotency: Safe to retry; read-only."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Issue ID (e.g. 'br-1a2b3c'). MUST be an exact ID from list_issues. Placeholder values are rejected."
                    },
                    "ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": SHOW_ISSUE_BATCH_MAX,
                        "description": "Batch of exact issue IDs. Returns {items,count,ok_count,error_count}; each item is either {id,ok:true,issue} or {id,ok:false,error}. Partial failures do not fail the whole batch."
                    }
                },
                "oneOf": [
                    {"required": ["id"]},
                    {"required": ["ids"]}
                ],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: Some(true),
                destructive: Some(false),
                idempotent: Some(true),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        if let Some(ids) = show_issue_batch_ids(&args)? {
            let key = format!("tool:show_issue_batch:{}", json!(&ids));
            let result = cached_read_json(&self.0, key, |storage| {
                Ok(show_issue_batch_json(storage, &ids))
            })?;
            return Ok(vec![Content::text(result.to_string())]);
        }

        let id = required_str_arg(&args, "id")?;

        // Placeholder detection before any DB work
        if let Some(err) = detect_placeholder(&id) {
            return Err(err);
        }

        let key = format!("tool:show_issue:{id}");
        let result = cached_read_json(&self.0, key, |storage| show_issue_json(storage, &id))?;
        Ok(vec![Content::text(result.to_string())])
    }
}

// ---------------------------------------------------------------------------
// 3. create_issue
// ---------------------------------------------------------------------------

pub struct CreateIssueTool(Arc<BeadsState>);
impl CreateIssueTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn create_issue_result_json(
    id: &str,
    title: &str,
    priority: Priority,
    issue_type: &IssueType,
    parent_id: Option<&str>,
    coercions: &[String],
) -> Value {
    let mut result = json!({
        "id": id,
        "title": title,
        "status": "open",
        "priority": priority.0,
        "type": issue_type.as_str(),
        "next_actions": [
            "Use update_issue to add details or change fields",
            "Use manage_dependencies to link to other issues"
        ]
    });

    if let Some(pid) = parent_id {
        result["parent"] = json!(pid);
    }

    if !coercions.is_empty() {
        result["coercions"] = json!(coercions);
        result["warnings"] = result["coercions"].clone();
    }

    result
}

fn create_issue_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    args: &Value,
) -> McpResult<Value> {
    let title = required_str_arg(args, "title")?;
    validate_mcp_title(&title)?;

    let mut coercions: Vec<String> = Vec::new();

    let (issue_type, type_warning) = optional_str_arg(args, "type")?
        .as_deref()
        .map_or((IssueType::Task, None), parse_issue_type);
    if let Some(w) = type_warning {
        coercions.push(w);
    }

    let (priority, prio_warning) = optional_str_arg(args, "priority")?
        .as_deref()
        .map(parse_priority)
        .transpose()?
        .unwrap_or((Priority::MEDIUM, None));
    if let Some(w) = prio_warning {
        coercions.push(w);
    }

    let parent_id = optional_str_arg(args, "parent")?;
    let description = optional_str_arg(args, "description")?;
    let assignee = optional_str_arg(args, "assignee")?;
    let labels_to_add = optional_label_array_arg(args, "labels")?;

    let now = chrono::Utc::now();
    let prefix = state.issue_prefix.as_deref().unwrap_or("br");

    if let Some(ref pid) = parent_id {
        require_valid_issue(storage, pid)?;
    }

    let id = if let Some(ref pid) = parent_id {
        let next_num = storage.next_child_number(pid).map_err(beads_to_mcp)?;
        next_available_child_id(pid, next_num, |candidate| storage.id_exists(candidate))?
    } else {
        let issue_count = storage.count_issues().map_err(beads_to_mcp)?;
        generate_issue_id_with_checked_lookup(
            &title,
            &state.actor,
            now,
            prefix,
            issue_count,
            |candidate| storage.id_exists(candidate),
        )?
    };

    let issue = Issue {
        id: id.clone(),
        title: title.clone(),
        description: description.clone(),
        status: Status::Open,
        priority,
        issue_type: issue_type.clone(),
        assignee: assignee.clone(),
        created_by: Some(state.actor.clone()),
        created_at: now,
        updated_at: now,
        ..Issue::default()
    };

    IssueValidator::validate(&issue)
        .map_err(BeadsError::from_validation_errors)
        .map_err(beads_to_mcp)?;

    storage
        .create_issue(&issue, &state.actor)
        .map_err(beads_to_mcp)?;

    for label in &labels_to_add {
        storage
            .add_label(&id, label, &state.actor)
            .map_err(beads_to_mcp)?;
    }

    if let Some(ref pid) = parent_id {
        storage
            .add_dependency(&id, pid, "parent-child", &state.actor)
            .map_err(beads_to_mcp)?;
    }

    Ok(create_issue_result_json(
        &id,
        &title,
        priority,
        &issue_type,
        parent_id.as_deref(),
        &coercions,
    ))
}

fn create_issue_batch_items(args: &Value) -> McpResult<Option<Vec<Value>>> {
    let Some(value) = args.get("issues") else {
        return Ok(None);
    };

    if [
        "title",
        "description",
        "type",
        "priority",
        "assignee",
        "labels",
        "parent",
    ]
    .iter()
    .any(|key| args.get(*key).is_some())
    {
        return Err(McpError::invalid_params(
            "Provide either title for a single issue or issues[] for a batch, not both",
        ));
    }

    let items = value.as_array().ok_or_else(|| {
        McpError::invalid_params(format!("'issues' must be an array of objects, got {value}"))
    })?;
    if items.is_empty() {
        return Err(McpError::invalid_params(
            "'issues' must include at least one issue object",
        ));
    }
    if items.len() > CREATE_ISSUE_BATCH_MAX {
        return Err(McpError::invalid_params(format!(
            "'issues' supports at most {CREATE_ISSUE_BATCH_MAX} issue objects per call, got {}",
            items.len()
        )));
    }
    for (idx, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(McpError::invalid_params(format!(
                "'issues[{idx}]' must be an object, got {item}"
            )));
        }
    }

    Ok(Some(items.clone()))
}

fn create_issue_batch_error_item(index: usize, args: &Value, err: McpError) -> Value {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map_or(Value::Null, |title| json!(title));
    json!({
        "index": index,
        "title": title,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn create_issue_batch_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    items: &[Value],
) -> Value {
    let mut ok_count = 0_u64;
    let results = items
        .iter()
        .enumerate()
        .map(
            |(index, item)| match create_issue_json(storage, state, item) {
                Ok(result) => {
                    ok_count += 1;
                    json!({
                        "index": index,
                        "title": item.get("title").and_then(Value::as_str),
                        "id": result.get("id").and_then(Value::as_str),
                        "ok": true,
                        "result": result,
                    })
                }
                Err(err) => create_issue_batch_error_item(index, item, err),
            },
        )
        .collect::<Vec<_>>();
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);

    json!({
        "items": results,
        "count": count,
        "ok_count": ok_count,
        "error_count": count.saturating_sub(ok_count),
    })
}

impl ToolHandler for CreateIssueTool {
    #[allow(clippy::too_many_lines)]
    fn definition(&self) -> Tool {
        Tool {
            name: "create_issue".into(),
            description: Some(
                "Create one issue, or create multiple issues in one batch. Returns created issue IDs.\n\n\
                 Discovery: See beads://schema for valid types/priorities, beads://labels for labels.\n\
                 When to use: Recording a new bug, feature, task, or work item.\n\
                 NOT for: Updating existing issues — use update_issue instead.\n\
                 Do: Provide a clear title (1-500 chars), or pass issues[] for a per-item batch envelope. Search with list_issues first to avoid dupes.\n\
                 Don't: Create duplicate issues — search first.\n\
                 Inputs auto-corrected: 'urgent' → critical, 'feat' → feature, etc.\n\
                 Batch semantics: issues[] uses one write lock/storage open/auto-flush and returns {items,count,ok_count,error_count}; each item has ok:true with the legacy result or ok:false with a structured error.\n\
                 Idempotency: NOT idempotent — each call creates a new issue."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Issue title (1-500 chars). REQUIRED."
                    },
                    "description": {
                        "type": "string",
                        "description": "Detailed description of the issue"
                    },
                    "type": {
                        "type": "string",
                        "description": "Issue type: task (default), bug, feature, epic, chore, docs, question. Aliases: feat, defect, enhancement, refactor, doc."
                    },
                    "priority": {
                        "type": "string",
                        "description": "Priority: critical, high, medium (default), low, backlog. Aliases: urgent, important, normal, minor, someday."
                    },
                    "assignee": {
                        "type": "string",
                        "description": "Assign to a user"
                    },
                    "labels": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Labels to attach. See beads://labels for existing labels."
                    },
                    "parent": {
                        "type": "string",
                        "description": "Parent issue ID to create as sub-issue. Creates a parent-child dependency automatically."
                    },
                    "issues": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": CREATE_ISSUE_BATCH_MAX,
                        "description": "Batch of issue objects using the same fields as a single create item: title, description, type, priority, assignee, labels, and parent. Returns {items,count,ok_count,error_count}; each item is either {index,title,id,ok:true,result} using the legacy single-create result shape, or {index,title,ok:false,error}. Partial failures do not fail the whole batch.",
                        "items": {
                            "type": "object",
                            "required": ["title"],
                            "additionalProperties": true
                        }
                    }
                },
                "oneOf": [
                    {"required": ["title"]},
                    {"required": ["issues"]}
                ],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: None,
                destructive: Some(false),
                idempotent: Some(false),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        if let Some(items) = create_issue_batch_items(&args)? {
            let result = self
                .0
                .with_mutation(|storage| Ok(create_issue_batch_json(storage, &self.0, &items)))?;
            return Ok(vec![Content::text(result.to_string())]);
        }

        let result = self
            .0
            .with_mutation(|storage| create_issue_json(storage, &self.0, &args))?;

        Ok(vec![Content::text(result.to_string())])
    }
}

// ---------------------------------------------------------------------------
// 4. update_issue
// ---------------------------------------------------------------------------

pub struct UpdateIssueTool(Arc<BeadsState>);
impl UpdateIssueTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn update_issue_result_json(issue: &Issue, coercions: &[String]) -> Value {
    let mut result = json!({
        "id": &issue.id,
        "title": &issue.title,
        "status": &issue.status,
        "priority": &issue.priority,
        "updated_at": &issue.updated_at,
    });

    if !coercions.is_empty() {
        result["coercions"] = json!(coercions);
        result["warnings"] = result["coercions"].clone();
    }

    result
}

/// The in-place acceptance-checklist edit an `update_issue` call asked for,
/// validated against the issue's current field before any write (GitHub
/// #477). `changed` is false when every requested item was already in the
/// requested state and nothing was appended; the field is then left alone.
struct McpAcceptancePlan {
    edit: AcceptanceEdit,
    changed: bool,
}

fn plan_mcp_acceptance_edit(
    storage: &SqliteStorage,
    id: &str,
    args: &Value,
) -> McpResult<Option<McpAcceptancePlan>> {
    let check = optional_string_array_arg(args, "check_acceptance")?;
    let uncheck = optional_string_array_arg(args, "uncheck_acceptance")?;
    let append = optional_string_array_arg(args, "add_acceptance")?;
    if check.is_empty() && uncheck.is_empty() && append.is_empty() {
        return Ok(None);
    }
    let body = storage
        .get_issue(id)
        .map_err(beads_to_mcp)?
        .and_then(|issue| issue.acceptance_criteria)
        .unwrap_or_default();
    let edit = plan_acceptance_edit(&body, &check, &uncheck, &append).map_err(|err| {
        McpError::with_data(
            McpErrorCode::InvalidParams,
            format!("{}: {}", err.field, err.reason),
            json!({
                "error_type": "INVALID_ACCEPTANCE_EDIT",
                "recoverable": true,
                "field": err.field,
                "hint": "Select items by 1-based index or unique item text; show_issue lists them under acceptance_items",
                "suggested_tool_calls": [
                    {"tool": "show_issue", "arguments": {"id": id}}
                ]
            }),
        )
    })?;
    let changed = edit.changed_from(&body);
    Ok(Some(McpAcceptancePlan { edit, changed }))
}

fn apply_update_issue_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    args: &Value,
) -> McpResult<Value> {
    let id = required_str_arg(args, "id")?;

    let (mut updates, coercions) = parse_update_fields(&id, args)?;
    let labels_to_add = optional_label_array_arg(args, "labels_add")?;
    let labels_to_remove = optional_label_array_arg(args, "labels_remove")?;
    let comment = optional_str_arg(args, "comment")?;

    if let Some(comment) = comment.as_deref()
        && !comment.is_empty()
    {
        validate_mcp_comment(&id, &state.actor, comment)?;
    }

    // Validate ID exists before attempting update (placeholder + existence check).
    require_valid_issue(storage, &id)?;

    let acceptance = plan_mcp_acceptance_edit(storage, &id, args)?;
    let acceptance_rewrite = acceptance.as_ref().is_some_and(|plan| plan.changed);
    if let Some(plan) = acceptance.as_ref().filter(|plan| plan.changed) {
        updates.acceptance_criteria = Some(Some(plan.edit.body.clone()));
    }

    let has_field_updates =
        acceptance_rewrite || UPDATE_FIELD_KEYS.iter().any(|k| args.get(k).is_some());
    let has_side_effects = !labels_to_add.is_empty()
        || !labels_to_remove.is_empty()
        || comment.as_deref().is_some_and(|s| !s.is_empty());

    let issue = if has_field_updates {
        storage
            .update_issue(&id, &updates, &state.actor)
            .map_err(beads_to_mcp)?
    } else if has_side_effects || acceptance.is_some() {
        match storage
            .get_issue_details(&id, false, false, 0)
            .map_err(beads_to_mcp)?
        {
            Some(details) => details.issue,
            None => return Err(issue_not_found_err(storage, &id)?),
        }
    } else {
        return Err(McpError::with_data(
            McpErrorCode::ToolExecutionError,
            "No changes specified",
            json!({
                "error_type": "NOTHING_TO_DO",
                "recoverable": true,
                "hint": "Provide at least one field to update, an acceptance checklist edit, a label operation, or a comment",
                "suggested_tool_calls": [
                    {"tool": "show_issue", "arguments": {"id": id}}
                ]
            }),
        ));
    };

    // Handle label mutations.
    for label in &labels_to_add {
        storage
            .add_label(&id, label, &state.actor)
            .map_err(beads_to_mcp)?;
    }
    for label in &labels_to_remove {
        storage
            .remove_label(&id, label, &state.actor)
            .map_err(beads_to_mcp)?;
    }

    // Add comment if provided.
    if let Some(comment) = comment.as_deref()
        && !comment.is_empty()
    {
        storage
            .add_comment(&id, &state.actor, comment)
            .map_err(beads_to_mcp)?;
    }

    let mut result = update_issue_result_json(&issue, &coercions);
    if let Some(plan) = acceptance {
        let mut report = serde_json::to_value(AcceptanceCriteriaOutput::from_edit(&plan.edit))
            .unwrap_or_default();
        if let Some(obj) = report.as_object_mut() {
            obj.insert("changed".into(), json!(plan.changed));
        }
        result["acceptance_criteria"] = report;
    }
    Ok(result)
}

fn update_issue_batch_items(args: &Value) -> McpResult<Option<Vec<Value>>> {
    if args.get("id").is_some() && args.get("updates").is_some() {
        return Err(McpError::invalid_params(
            "Provide either 'id' for a single update or 'updates' for a batch, not both",
        ));
    }

    let Some(value) = args.get("updates") else {
        return Ok(None);
    };

    let items = value.as_array().ok_or_else(|| {
        McpError::invalid_params(format!(
            "'updates' must be an array of objects, got {value}"
        ))
    })?;
    if items.is_empty() {
        return Err(McpError::invalid_params(
            "'updates' must include at least one update item",
        ));
    }
    if items.len() > UPDATE_ISSUE_BATCH_MAX {
        return Err(McpError::invalid_params(format!(
            "'updates' supports at most {UPDATE_ISSUE_BATCH_MAX} update items per call, got {}",
            items.len()
        )));
    }
    for (idx, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(McpError::invalid_params(format!(
                "'updates[{idx}]' must be an object, got {item}"
            )));
        }
    }

    Ok(Some(items.clone()))
}

fn update_issue_batch_error_item(index: usize, args: &Value, err: McpError) -> Value {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map_or(Value::Null, |id| json!(id));
    json!({
        "index": index,
        "id": id,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn update_issue_batch_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    items: &[Value],
) -> Value {
    let mut ok_count = 0_u64;
    let results = items
        .iter()
        .enumerate()
        .map(
            |(index, item)| match apply_update_issue_json(storage, state, item) {
                Ok(result) => {
                    ok_count += 1;
                    json!({
                        "index": index,
                        "id": result["id"].clone(),
                        "ok": true,
                        "result": result,
                    })
                }
                Err(err) => update_issue_batch_error_item(index, item, err),
            },
        )
        .collect::<Vec<_>>();
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);

    json!({
        "items": results,
        "count": count,
        "ok_count": ok_count,
        "error_count": count.saturating_sub(ok_count),
    })
}

impl ToolHandler for UpdateIssueTool {
    #[allow(clippy::too_many_lines)]
    fn definition(&self) -> Tool {
        Tool {
            name: "update_issue".into(),
            description: Some(
                "Update fields on an existing issue. Only provided fields are changed.\n\n\
                 Discovery: Get IDs from list_issues. See beads://schema for valid values.\n\
                 When to use: Changing status, priority, assignee, adding comments.\n\
                 NOT for: Closing issues — use close_issue for proper close tracking.\n\
                 Do: Provide only the fields you want to change, or pass updates[] for a per-item batch envelope.\n\
                 Don't: Set status to 'closed' — you'll be redirected to close_issue.\n\
                 Inputs auto-corrected: 'wip' → in_progress, 'urgent' → critical, etc.\n\
                 Batch semantics: updates[] uses one write lock/storage open/auto-flush and returns {items,count,ok_count,error_count}; each item has ok:true with the legacy result or ok:false with a structured error.\n\
                 Idempotency: Safe to retry with the same values."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Issue ID to update. REQUIRED. Must be a real ID from list_issues."
                    },
                    "title": {
                        "type": "string",
                        "description": "New title (1-500 chars)"
                    },
                    "description": {
                        "type": ["string", "null"],
                        "description": "New description (null to clear)"
                    },
                    "status": {
                        "type": "string",
                        "description": "New status: open, in_progress, blocked, deferred, draft, pinned. NOT 'closed' — use close_issue instead. Aliases accepted."
                    },
                    "priority": {
                        "type": "string",
                        "description": "New priority: critical, high, medium, low, backlog. Aliases: urgent, important, normal, minor, someday."
                    },
                    "type": {
                        "type": "string",
                        "description": "New issue type: task, bug, feature, epic, chore, docs, question. Aliases accepted."
                    },
                    "assignee": {
                        "type": ["string", "null"],
                        "description": "New assignee (null to unassign)"
                    },
                    "owner": {
                        "type": ["string", "null"],
                        "description": "New owner (null to clear). Owner is the person responsible, assignee does the work."
                    },
                    "due_at": {
                        "type": ["string", "null"],
                        "description": "Due date (ISO 8601 or 'YYYY-MM-DD'). Null to clear. Auto-coerced: slashes → dashes, missing timezone → UTC."
                    },
                    "defer_until": {
                        "type": ["string", "null"],
                        "description": "Defer until date (ISO 8601 or 'YYYY-MM-DD'). Null to clear. Issue will be deferred until this date."
                    },
                    "estimated_minutes": {
                        "type": ["integer", "null"],
                        "description": "Estimated effort in minutes. Null to clear."
                    },
                    "external_ref": {
                        "type": ["string", "null"],
                        "description": "External tracker reference (e.g. GitHub issue URL). Null to clear."
                    },
                    "check_acceptance": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Tick acceptance checklist items in place without rewriting acceptance_criteria: 1-based item numbers (\"1,4\") or unique item text. show_issue lists items under acceptance_items."
                    },
                    "uncheck_acceptance": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Untick acceptance checklist items in place (same selectors as check_acceptance)."
                    },
                    "add_acceptance": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Append unchecked '- [ ] TEXT' criteria to acceptance_criteria without rewriting the field."
                    },
                    "labels_add": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Labels to add. See beads://labels for existing labels."
                    },
                    "labels_remove": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Labels to remove"
                    },
                    "comment": {
                        "type": "string",
                        "description": "Add a comment to the issue (appended after field updates)"
                    },
                    "updates": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": UPDATE_ISSUE_BATCH_MAX,
                        "description": "Batch of update objects using the same fields as a single update item: id plus optional title, description, status, priority, type, assignee, owner, due_at, defer_until, estimated_minutes, external_ref, labels_add, labels_remove, and comment. Returns {items,count,ok_count,error_count}; each item is either {index,id,ok:true,result} using the legacy single-update result shape, or {index,id,ok:false,error}. Partial failures do not fail the whole batch.",
                        "items": {
                            "type": "object",
                            "required": ["id"],
                            "additionalProperties": true
                        }
                    }
                },
                "oneOf": [
                    {"required": ["id"]},
                    {"required": ["updates"]}
                ],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: None,
                destructive: Some(false),
                idempotent: Some(true),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        if let Some(items) = update_issue_batch_items(&args)? {
            let result = self
                .0
                .with_mutation(|storage| Ok(update_issue_batch_json(storage, &self.0, &items)))?;
            return Ok(vec![Content::text(result.to_string())]);
        }

        let result = self
            .0
            .with_mutation(|storage| apply_update_issue_json(storage, &self.0, &args))?;
        Ok(vec![Content::text(result.to_string())])
    }
}

// ---------------------------------------------------------------------------
// 5. close_issue
// ---------------------------------------------------------------------------

pub struct CloseIssueTool(Arc<BeadsState>);
impl CloseIssueTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn close_issue_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    id: &str,
    reason: Option<&str>,
) -> McpResult<Value> {
    // Validate ID exists (with placeholder detection + fuzzy suggestions).
    require_valid_issue(storage, id)?;

    // Idempotency: if already closed, return existing state without error.
    if let Some(details) = storage
        .get_issue_details(id, false, false, 0)
        .map_err(beads_to_mcp)?
        && details.issue.status == Status::Closed
    {
        let issue = details.issue;
        return Ok(json!({
            "id": issue.id,
            "title": issue.title,
            "status": "closed",
            "closed_at": issue.closed_at,
            "close_reason": issue.close_reason,
            "already_closed": true,
            "next_actions": ["Issue was already closed. Use list_issues to find open work."]
        }));
    }

    let now = chrono::Utc::now();
    let close_update = IssueUpdate {
        status: Some(Status::Closed),
        closed_at: Some(Some(now)),
        close_reason: Some(reason.map(str::to_string)),
        ..IssueUpdate::default()
    };

    let issue = storage
        .update_issue(id, &close_update, &state.actor)
        .map_err(beads_to_mcp)?;

    let mut warnings = Vec::new();

    // Check for blockers this issue had (warn about closing a blocked issue).
    let our_blockers = match storage.get_blockers(id) {
        Ok(blockers) => Some(blockers),
        Err(err) => {
            warnings.push(storage_read_warning("get_blockers", &err));
            None
        }
    };

    // Check what this issue was blocking (now potentially unblocked).
    let dependents = match storage.get_blocked_issue_ids(id) {
        Ok(dependents) => Some(dependents),
        Err(err) => {
            warnings.push(storage_read_warning("get_blocked_issue_ids", &err));
            None
        }
    };

    let mut result = json!({
        "id": issue.id,
        "title": issue.title,
        "status": "closed",
        "closed_at": issue.closed_at,
        "close_reason": reason,
    });

    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }

    if let Some(our_blockers) = our_blockers
        && !our_blockers.is_empty()
    {
        result["warning"] = json!(format!(
            "This issue was blocked by {} issue(s): {}. Consider whether those blockers are still relevant.",
            our_blockers.len(),
            our_blockers.join(", ")
        ));
    }

    if let Some(dependents) = dependents {
        if dependents.is_empty() {
            result["next_actions"] = json!(["Issue closed successfully."]);
        } else {
            result["unblocked_candidates"] = json!(dependents);
            result["next_actions"] = json!([
                format!(
                    "{} dependent issue(s) may now be unblocked: {}",
                    dependents.len(),
                    dependents.join(", ")
                ),
                "Use show_issue on these to check if they're now ready for work"
            ]);
        }
    } else {
        result["next_actions"] =
            json!(["Issue closed successfully. Dependent lookup failed; see warnings."]);
    }

    Ok(result)
}

fn close_issue_batch_ids(args: &Value) -> McpResult<Option<Vec<String>>> {
    if args.get("id").is_some() && args.get("ids").is_some() {
        return Err(McpError::invalid_params(
            "Provide either 'id' for a single close or 'ids' for a batch, not both",
        ));
    }

    optional_non_empty_string_array_arg(args, "ids", CLOSE_ISSUE_BATCH_MAX)
}

fn close_issue_batch_error_item(index: usize, id: &str, err: McpError) -> Value {
    json!({
        "index": index,
        "id": id,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn close_issue_batch_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    ids: &[String],
    reason: Option<&str>,
) -> Value {
    let mut ok_count = 0_u64;
    let items = ids
        .iter()
        .enumerate()
        .map(
            |(index, id)| match close_issue_json(storage, state, id, reason) {
                Ok(result) => {
                    ok_count += 1;
                    json!({
                        "index": index,
                        "id": id,
                        "ok": true,
                        "result": result,
                    })
                }
                Err(err) => close_issue_batch_error_item(index, id, err),
            },
        )
        .collect::<Vec<_>>();
    let count = u64::try_from(ids.len()).unwrap_or(u64::MAX);

    json!({
        "items": items,
        "count": count,
        "ok_count": ok_count,
        "error_count": count.saturating_sub(ok_count),
    })
}

impl ToolHandler for CloseIssueTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "close_issue".into(),
            description: Some(
                "Close one issue, or close multiple issues with a shared reason. Sets status to Closed and records close metadata.\n\n\
                 Discovery: Get IDs from list_issues.\n\
                 When to use: Completing, cancelling, or resolving an issue.\n\
                 NOT for: Changing status to anything other than closed — use update_issue.\n\
                 Do: Provide id for the legacy single-issue response, or ids[] for a per-item batch envelope.\n\
                 Don't: Close issues without checking open blockers first.\n\
                 Batch semantics: ids[] uses one write lock/storage open/auto-flush and returns {items,count,ok_count,error_count}; each item has ok:true with the legacy result or ok:false with a structured error.\n\
                 Idempotency: Safe to retry — closing an already-closed issue is a no-op."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Issue ID to close. REQUIRED. Must be a real ID from list_issues."
                    },
                    "ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": CLOSE_ISSUE_BATCH_MAX,
                        "items": {"type": "string"},
                        "description": "Batch of issue IDs to close with the same reason. Returns {items,count,ok_count,error_count}; each item is either {index,id,ok:true,result} using the legacy single-close result shape, or {index,id,ok:false,error}. Partial failures do not fail the whole batch."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why this issue is being closed (e.g. 'completed', 'wontfix', 'duplicate')"
                    }
                },
                "oneOf": [
                    {"required": ["id"]},
                    {"required": ["ids"]}
                ],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: None,
                destructive: Some(false),
                idempotent: Some(true),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        let reason = optional_str_arg(&args, "reason")?;
        if let Some(ids) = close_issue_batch_ids(&args)? {
            let result = self.0.with_mutation(|storage| {
                Ok(close_issue_batch_json(
                    storage,
                    &self.0,
                    &ids,
                    reason.as_deref(),
                ))
            })?;
            return Ok(vec![Content::text(result.to_string())]);
        }

        let id = required_str_arg(&args, "id")?;
        let result = self
            .0
            .with_mutation(|storage| close_issue_json(storage, &self.0, &id, reason.as_deref()))?;

        Ok(vec![Content::text(result.to_string())])
    }
}

// ---------------------------------------------------------------------------
// 6. manage_dependencies
// ---------------------------------------------------------------------------

pub struct ManageDependenciesTool(Arc<BeadsState>);
impl ManageDependenciesTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

fn required_dependency_action_arg(args: &Value) -> McpResult<String> {
    required_str_arg(args, "action").map_err(|err| {
        McpError::with_data(
            McpErrorCode::InvalidParams,
            err.to_string(),
            json!({
                "error_type": "REQUIRED_FIELD",
                "available_options": ["add", "remove", "list"],
                "fix_hint": "Provide action: 'list' to view, 'add' to create, 'remove' to delete"
            }),
        )
    })
}

fn invalid_dependency_action_error(action: &str) -> McpError {
    McpError::with_data(
        McpErrorCode::InvalidParams,
        format!("Unknown action '{action}'"),
        json!({
            "error_type": "INVALID_ARGUMENT",
            "provided": action,
            "available_options": ["add", "remove", "list"],
            "fix_hint": "Use 'list' to view dependencies, 'add' to create, 'remove' to delete"
        }),
    )
}

fn manage_dependencies_list_json(storage: &SqliteStorage, id: &str) -> McpResult<Value> {
    require_valid_issue(storage, id)?;
    let deps = storage.get_dependencies_full(id).map_err(beads_to_mcp)?;
    let dependents = storage.get_dependents(id).map_err(beads_to_mcp)?;

    Ok(json!({
        "id": id,
        "depends_on": deps.iter().map(|d| {
            json!({
                "id": d.depends_on_id,
                "dep_type": d.dep_type.to_string(),
            })
        }).collect::<Vec<_>>(),
        "depended_on_by": dependents,
    }))
}

fn manage_dependency_cycle_error(id: &str, depends_on: &str) -> McpError {
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        format!("Adding dependency {id} -> {depends_on} would create a cycle"),
        json!({
            "error_type": "CYCLE_DETECTED",
            "recoverable": false,
            "from": id,
            "to": depends_on,
            "hint": "Circular dependencies are not allowed. Check the existing dependency graph.",
            "suggested_tool_calls": [
                {"tool": "manage_dependencies", "arguments": {"action": "list", "id": id}},
                {"tool": "manage_dependencies", "arguments": {"action": "list", "id": depends_on}}
            ]
        }),
    )
}

fn manage_dependencies_add_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    id: &str,
    depends_on: &str,
    dep_type_raw: Option<&str>,
) -> McpResult<Value> {
    let dep_type_raw = dep_type_raw.unwrap_or("blocks");
    let (dep_type_str, dep_coercion) = parse_dep_type(dep_type_raw)?;
    let dep_type = dep_type_str
        .parse::<DependencyType>()
        .map_err(beads_to_mcp)?;

    require_valid_issue(storage, id)?;
    if depends_on.starts_with("external:") {
        if let Some(err) = detect_placeholder(depends_on) {
            return Err(err);
        }
    } else {
        require_valid_issue(storage, depends_on)?;
    }

    let would_create_cycle = if dep_type.is_blocking() && !depends_on.starts_with("external:") {
        if dep_type == DependencyType::ParentChild {
            storage
                .would_create_parent_child_cycle(id, depends_on, true)
                .map_err(beads_to_mcp)?
        } else {
            storage
                .would_create_cycle(id, depends_on, true)
                .map_err(beads_to_mcp)?
        }
    } else {
        false
    };

    if would_create_cycle {
        return Err(manage_dependency_cycle_error(id, depends_on));
    }

    let added = storage
        .add_dependency(id, depends_on, &dep_type_str, &state.actor)
        .map_err(beads_to_mcp)?;

    let mut result = json!({
        "added": added,
        "from": id,
        "to": depends_on,
        "dep_type": dep_type_str,
    });
    if let Some(w) = dep_coercion {
        result["coercion"] = json!(w);
    }

    Ok(result)
}

fn manage_dependencies_remove_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    id: &str,
    depends_on: &str,
) -> McpResult<Value> {
    if let Some(err) = detect_placeholder(depends_on) {
        return Err(err);
    }

    require_valid_issue(storage, id)?;

    let removed = storage
        .remove_dependency(id, depends_on, &state.actor)
        .map_err(beads_to_mcp)?;

    Ok(json!({
        "removed": removed,
        "from": id,
        "to": depends_on,
    }))
}

fn manage_dependencies_operation_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    args: &Value,
) -> McpResult<Value> {
    let action = required_dependency_action_arg(args)?;
    let id = required_str_arg(args, "id")?;

    match action.as_str() {
        "list" => manage_dependencies_list_json(storage, &id),
        "add" => {
            let depends_on = required_str_arg(args, "depends_on")
                .map_err(|err| McpError::invalid_params(format!("{err} for action 'add'")))?;
            let dep_type_raw = optional_str_arg(args, "dep_type")?;
            manage_dependencies_add_json(storage, state, &id, &depends_on, dep_type_raw.as_deref())
        }
        "remove" => {
            let depends_on = required_str_arg(args, "depends_on")
                .map_err(|err| McpError::invalid_params(format!("{err} for action 'remove'")))?;
            manage_dependencies_remove_json(storage, state, &id, &depends_on)
        }
        other => Err(invalid_dependency_action_error(other)),
    }
}

fn manage_dependencies_batch_items(args: &Value) -> McpResult<Option<Vec<Value>>> {
    let Some(value) = args.get("operations") else {
        return Ok(None);
    };

    if ["action", "id", "depends_on", "dep_type"]
        .iter()
        .any(|key| args.get(*key).is_some())
    {
        return Err(McpError::invalid_params(
            "Provide either action/id for a single dependency operation or operations[] for a batch, not both",
        ));
    }

    let items = value.as_array().ok_or_else(|| {
        McpError::invalid_params(format!(
            "'operations' must be an array of objects, got {value}"
        ))
    })?;
    if items.is_empty() {
        return Err(McpError::invalid_params(
            "'operations' must include at least one dependency operation",
        ));
    }
    if items.len() > MANAGE_DEPENDENCIES_BATCH_MAX {
        return Err(McpError::invalid_params(format!(
            "'operations' supports at most {MANAGE_DEPENDENCIES_BATCH_MAX} items per call, got {}",
            items.len()
        )));
    }
    for (idx, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(McpError::invalid_params(format!(
                "'operations[{idx}]' must be an object, got {item}"
            )));
        }
    }

    Ok(Some(items.clone()))
}

fn manage_dependencies_batch_is_read_only(items: &[Value]) -> bool {
    items
        .iter()
        .all(|item| item.get("action").and_then(Value::as_str) == Some("list"))
}

fn manage_dependencies_batch_error_item(index: usize, args: &Value, err: McpError) -> Value {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map_or(Value::Null, |action| json!(action));
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map_or(Value::Null, |id| json!(id));
    json!({
        "index": index,
        "action": action,
        "id": id,
        "ok": false,
        "error": mcp_error_json(err),
    })
}

fn manage_dependencies_batch_json(
    storage: &mut SqliteStorage,
    state: &BeadsState,
    items: &[Value],
) -> Value {
    let mut ok_count = 0_u64;
    let results = items
        .iter()
        .enumerate()
        .map(
            |(index, item)| match manage_dependencies_operation_json(storage, state, item) {
                Ok(result) => {
                    ok_count += 1;
                    json!({
                        "index": index,
                        "action": item.get("action").and_then(Value::as_str),
                        "id": item.get("id").and_then(Value::as_str),
                        "ok": true,
                        "result": result,
                    })
                }
                Err(err) => manage_dependencies_batch_error_item(index, item, err),
            },
        )
        .collect::<Vec<_>>();
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);

    json!({
        "items": results,
        "count": count,
        "ok_count": ok_count,
        "error_count": count.saturating_sub(ok_count),
    })
}

impl ToolHandler for ManageDependenciesTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "manage_dependencies".into(),
            description: Some(
                "Add, remove, or list dependencies between issues, with optional batching for graph-edit bursts.\n\n\
                 Discovery: Get issue IDs from list_issues. See beads://schema for dep types.\n\
                 When to use: Linking related issues, establishing blocking relationships.\n\
                 NOT for: Viewing blocked issues overview — use beads://issues/blocked resource.\n\
                 Do: Use 'list' action first to see existing deps before modifying, or operations[] for ordered batch work.\n\
                 Don't: Create circular deps — the system will reject them with guidance.\n\
                 Common mistakes: Swapping source/target for 'blocks' type; using placeholder IDs.\n\
                 Batch semantics: operations[] uses one storage envelope and returns {items,count,ok_count,error_count}; each item has ok:true with the legacy result or ok:false with a structured error.\n\
                 Both IDs are validated before any operation."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "remove", "list"],
                        "description": "Action to perform. REQUIRED."
                    },
                    "id": {
                        "type": "string",
                        "description": "Source issue ID. REQUIRED. Must be a real ID from list_issues."
                    },
                    "depends_on": {
                        "type": "string",
                        "description": "Target issue ID or external:<project>:<capability> reference (required for add/remove)."
                    },
                    "dep_type": {
                        "type": "string",
                        "description": "Dependency type: blocks (default), related, parent-child, waits-for, duplicates, supersedes, caused-by. Aliases auto-corrected (e.g. 'parent_child' → 'parent-child').",
                        "default": "blocks"
                    },
                    "operations": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MANAGE_DEPENDENCIES_BATCH_MAX,
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "enum": ["add", "remove", "list"]
                                },
                                "id": {"type": "string"},
                                "depends_on": {"type": "string"},
                                "dep_type": {
                                    "type": "string",
                                    "default": "blocks"
                                }
                            },
                            "required": ["action", "id"],
                            "additionalProperties": false
                        },
                        "description": "Batch of dependency operations. Returns {items,count,ok_count,error_count}; each item is either {index,action,id,ok:true,result} using the legacy single-operation result shape, or {index,action,id,ok:false,error}. Partial failures do not fail the whole batch."
                    }
                },
                "oneOf": [
                    {"required": ["action", "id"]},
                    {"required": ["operations"]}
                ],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: None,
                destructive: Some(false),
                // Mixed batch: list is read-only, add is idempotent, remove is
                // not — so no single idempotent hint applies. The tool only
                // touches the local workspace (closed world).
                idempotent: None,
                open_world_hint: Some(false),
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        if let Some(items) = manage_dependencies_batch_items(&args)? {
            let result = if manage_dependencies_batch_is_read_only(&items) {
                let mut storage = open(&self.0)?;
                manage_dependencies_batch_json(&mut storage, &self.0, &items)
            } else {
                self.0.with_mutation(|storage| {
                    Ok(manage_dependencies_batch_json(storage, &self.0, &items))
                })?
            };
            return Ok(vec![Content::text(result.to_string())]);
        }

        let action = required_dependency_action_arg(&args)?;
        let id = required_str_arg(&args, "id")?;
        match action.as_str() {
            "list" => {
                let storage = open(&self.0)?;
                let result = manage_dependencies_list_json(&storage, &id)?;
                Ok(vec![Content::text(result.to_string())])
            }
            "add" => {
                let depends_on = required_str_arg(&args, "depends_on")
                    .map_err(|err| McpError::invalid_params(format!("{err} for action 'add'")))?;
                let dep_type_raw = optional_str_arg(&args, "dep_type")?;
                let result = self.0.with_mutation(|storage| {
                    manage_dependencies_add_json(
                        storage,
                        &self.0,
                        &id,
                        &depends_on,
                        dep_type_raw.as_deref(),
                    )
                })?;
                Ok(vec![Content::text(result.to_string())])
            }
            "remove" => {
                let depends_on = required_str_arg(&args, "depends_on").map_err(|err| {
                    McpError::invalid_params(format!("{err} for action 'remove'"))
                })?;
                let removed = self.0.with_mutation(|storage| {
                    manage_dependencies_remove_json(storage, &self.0, &id, &depends_on)
                })?;
                Ok(vec![Content::text(removed.to_string())])
            }
            other => Err(invalid_dependency_action_error(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// 7. project_overview
// ---------------------------------------------------------------------------

fn project_overview_json(state: &BeadsState, storage: &SqliteStorage) -> McpResult<Value> {
    let total = storage.count_all_issues().map_err(beads_to_mcp)?;
    let active = storage.count_active_issues().map_err(beads_to_mcp)?;
    let labels = storage
        .get_unique_labels_with_counts()
        .map_err(beads_to_mcp)?;
    let blocked = storage.get_blocked_issues().map_err(beads_to_mcp)?;
    let dirty = storage.get_dirty_issue_count().map_err(beads_to_mcp)?;

    let ready = mcp_ready_issues(state, storage)?;

    // In-progress and deferred counts
    let in_progress_filters = ListFilters {
        statuses: Some(vec![Status::InProgress]),
        include_closed: false,
        limit: Some(50),
        ..ListFilters::default()
    };
    let in_progress = storage
        .list_issues(&in_progress_filters)
        .map_err(beads_to_mcp)?;

    let deferred_filters = ListFilters {
        statuses: Some(vec![Status::Deferred]),
        include_closed: false,
        include_deferred: true,
        limit: Some(50),
        ..ListFilters::default()
    };
    let deferred = storage
        .list_issues(&deferred_filters)
        .map_err(beads_to_mcp)?;

    let prefix = state.issue_prefix.as_deref().unwrap_or("br");

    Ok(json!({
        "project": {
            "beads_dir": state.beads_dir.display().to_string(),
            "issue_prefix": prefix,
        },
        "counts": {
            "total": total,
            "active": active,
            "blocked": blocked.len(),
            "ready": ready.len(),
            "in_progress": in_progress.len(),
            "deferred": deferred.len(),
            "dirty_unsaved": dirty,
        },
        "top_labels": labels.iter().take(15).map(|(name, count)| {
            json!({"label": name, "count": count})
        }).collect::<Vec<_>>(),
        "blocked_issues": blocked.iter().take(10).map(|(issue, blockers)| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "blocked_by": blockers,
            })
        }).collect::<Vec<_>>(),
        "ready_issues": ready.iter().take(10).map(|issue| {
            json!({
                "id": issue.id,
                "title": issue.title,
                "priority": issue.priority,
                "type": issue.issue_type,
            })
        }).collect::<Vec<_>>(),
        "discovery": {
            "resources": [
                "beads://schema — valid field values, aliases, and bead anatomy guidance",
                "beads://labels — all labels with counts",
                "beads://issues/ready — actionable work",
                "beads://issues/blocked — stuck items with blockers",
                "beads://coordination/status — stale-claim diagnosis using br.coordination.v1",
                "beads://issues/bottlenecks — highest-impact blockers (bv-style)",
                "beads://graph/health — dependency graph health metrics",
                "beads://issues/in_progress — current work",
                "beads://issues/deferred — deferred items",
                "beads://events/recent — latest audit trail",
                "beads://project/info — project metadata"
            ],
            "prompts": [
                "triage — guided backlog triage workflow",
                "status_report — project status report generation",
                "plan_next_work — graph-aware work planning (bottlenecks, quick wins)",
                "polish_backlog — review issue quality and dependency health"
            ]
        },
        "next_actions": [
            "Use list_issues to explore specific subsets",
            "Use show_issue to dig into a specific issue",
            "Use create_issue to add new work items"
        ]
    }))
}

pub struct ProjectOverviewTool(Arc<BeadsState>);
impl ProjectOverviewTool {
    pub fn new(state: Arc<BeadsState>) -> Self {
        Self(state)
    }
}

impl ToolHandler for ProjectOverviewTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "project_overview".into(),
            description: Some(
                "Get a high-level project summary: counts by status, top labels, blocked/ready work.\n\n\
                 Discovery: Pairs with beads://project/info, beads://schema, beads://issues/ready, \
                 and beads://coordination/status for the documented MCP orientation surface.\n\
                 When to use: Starting a session, getting oriented, understanding project health.\n\
                 NOT for: Detailed filtering — use list_issues with filters instead.\n\
                 Do: Call this first when you connect to understand the project state.\n\
                 Don't: Call repeatedly — the data doesn't change unless you mutate issues.\n\
                 Boundary: local SQLite/JSONL state only; no git, no network listener, and no live Agent Mail call.\n\
                 Idempotency: Safe to retry; read-only."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(ToolAnnotations {
                read_only: Some(true),
                destructive: Some(false),
                idempotent: Some(true),
                open_world_hint: None,
            }),
        }
    }

    fn call(&self, _ctx: &McpContext, args: serde_json::Value) -> McpResult<Vec<Content>> {
        ensure_not_shutting_down()?;

        let _ = args;
        let storage = self.0.open_read_storage().map_err(beads_to_mcp)?;
        let result = project_overview_json(&self.0, &storage)?;
        Ok(vec![Content::text(result.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseIssueTool, CreateIssueTool, ListIssuesTool, ManageDependenciesTool,
        ProjectOverviewTool, ShowIssueTool, UpdateIssueTool, build_list_filters,
        generate_issue_id_with_checked_lookup, issue_not_found_err, list_issues_batch_json,
        list_issues_json, next_available_child_id, optional_label_array_arg,
        optional_string_array_arg, parse_update_fields, project_overview_json,
        show_issue_batch_json, show_issue_json, storage_read_warning,
    };
    use crate::error::BeadsError;
    use crate::mcp::{BeadsState, McpReadSnapshotCache};
    use crate::model::{DependencyType, Issue, IssueType, Priority, Status};
    use crate::storage::{IssueUpdate, SqliteStorage};
    use chrono::{TimeZone, Utc};
    use fastmcp_rust::{Content, Cx, McpContext, McpErrorCode, Tool, ToolHandler};
    use serde_json::{Value, json};
    use std::{cell::Cell, fs, sync::Arc, time::Instant};
    use tempfile::TempDir;

    fn mcp_test_state(temp: &TempDir) -> Arc<BeadsState> {
        mcp_test_state_with_read_snapshot(temp, false)
    }

    fn mcp_test_state_with_read_snapshot(temp: &TempDir, read_snapshot: bool) -> Arc<BeadsState> {
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");
        SqliteStorage::open(&db_path).expect("initialize storage");
        Arc::new(BeadsState {
            db_path,
            beads_dir: beads_dir.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            // Generous enough to ride out a concurrent auto-flush holding
            // .write.lock under heavy parallel-test load; no test here asserts
            // the timeout path, so a short value only produces spurious
            // "Timed out waiting for write lock" flakiness.
            write_lock_timeout_ms: Some(5_000),
            allow_external_jsonl: false,
            actor: "mcp-test".to_string(),
            issue_prefix: Some("br".to_string()),
            history: crate::sync::history::HistoryConfig::default(),
            read_snapshot_cache: read_snapshot
                .then(|| std::sync::Mutex::new(McpReadSnapshotCache::default())),
        })
    }

    fn insert_test_issue(state: &BeadsState, id: &str, title: &str) {
        let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let now = Utc::now();
        let issue = Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        storage
            .create_issue(&issue, "mcp-test")
            .expect("create issue");
    }

    fn content_json(contents: &[Content]) -> serde_json::Value {
        let [Content::Text { text }] = contents else {
            return json!({"unexpected_content": format!("{contents:?}")});
        };
        serde_json::from_str(text)
            .unwrap_or_else(|err| json!({"parse_error": err.to_string(), "text": text}))
    }

    fn assert_docs_list_mcp_term(term: &str) {
        for (document_name, document) in [
            ("README.md", include_str!("../../README.md")),
            (
                "docs/AGENT_INTEGRATION.md",
                include_str!("../../docs/AGENT_INTEGRATION.md"),
            ),
            (
                "docs/CLI_REFERENCE.md",
                include_str!("../../docs/CLI_REFERENCE.md"),
            ),
        ] {
            assert!(
                document.contains(term),
                "{document_name} must document MCP term {term}"
            );
        }
    }

    fn assert_tool_description_mentions(tool: &Tool, expected_terms: &[&str]) {
        let description = tool
            .description
            .as_deref()
            .expect("MCP tool descriptions are part of the agent contract");
        assert!(!description.trim().is_empty());
        for term in expected_terms {
            assert!(
                description.contains(term),
                "MCP tool {} description must mention {term:?}: {description}",
                tool.name
            );
        }
    }

    fn assert_tool_description_does_not_claim_live_services(tool: &Tool) {
        let description = tool.description.as_deref().unwrap_or_default();
        let lower = description.to_ascii_lowercase();
        for forbidden in [
            "http://",
            "https://",
            "network port",
            "network socket",
            "network service",
        ] {
            assert!(
                !lower.contains(forbidden),
                "MCP tool {} description implies a live service boundary via {forbidden:?}: {description}",
                tool.name
            );
        }
        if lower.contains("agent mail") {
            assert!(
                lower.contains("no live agent mail")
                    || lower.contains("does not call")
                    || lower.contains("without"),
                "Agent Mail mention must be an explicit non-live boundary for {}: {description}",
                tool.name
            );
        }
        if lower.contains("network listener") {
            assert!(
                lower.contains("without network listener")
                    || lower.contains("no network listener")
                    || lower.contains("does not listen"),
                "network listener mention must be an explicit non-live boundary for {}: {description}",
                tool.name
            );
        }
    }

    fn assert_tool_input_schema_object(tool: &Tool) {
        assert_eq!(
            tool.input_schema["type"].as_str(),
            Some("object"),
            "MCP tool {} must expose an object input schema",
            tool.name
        );
        assert!(
            tool.input_schema.get("properties").is_some(),
            "MCP tool {} must expose input properties",
            tool.name
        );
        assert!(
            tool.input_schema.get("additionalProperties").is_some(),
            "MCP tool {} must make extra-argument behavior explicit",
            tool.name
        );
    }

    fn overview_resource_entries(overview: &Value) -> Vec<&str> {
        overview["discovery"]["resources"]
            .as_array()
            .expect("project_overview discovery.resources")
            .iter()
            .map(|value| value.as_str().expect("discovery resource entry"))
            .collect()
    }

    fn assert_batch_counts(batch: &serde_json::Value, count: u64, ok: u64, errors: u64) {
        assert_eq!(batch["count"].as_u64(), Some(count));
        assert_eq!(batch["ok_count"].as_u64(), Some(ok));
        assert_eq!(batch["error_count"].as_u64(), Some(errors));
    }

    #[test]
    fn mcp_contract_tool_metadata_matches_docs_and_discovery_contract() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let tools = vec![
            ListIssuesTool::new(Arc::clone(&state)).definition(),
            ShowIssueTool::new(Arc::clone(&state)).definition(),
            CreateIssueTool::new(Arc::clone(&state)).definition(),
            UpdateIssueTool::new(Arc::clone(&state)).definition(),
            CloseIssueTool::new(Arc::clone(&state)).definition(),
            ManageDependenciesTool::new(Arc::clone(&state)).definition(),
            ProjectOverviewTool::new(state).definition(),
        ];

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "list_issues",
                "show_issue",
                "create_issue",
                "update_issue",
                "close_issue",
                "manage_dependencies",
                "project_overview",
            ]
        );

        for tool in &tools {
            assert_docs_list_mcp_term(&tool.name);
            assert_tool_input_schema_object(tool);
            assert_tool_description_does_not_claim_live_services(tool);
        }

        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "list_issues")
                .expect("list_issues tool"),
            &["beads://schema", "beads://labels", "project_overview"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "show_issue")
                .expect("show_issue tool"),
            &["list_issues", "project_overview"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "create_issue")
                .expect("create_issue tool"),
            &["beads://schema", "beads://labels", "list_issues"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "update_issue")
                .expect("update_issue tool"),
            &["list_issues", "beads://schema"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "close_issue")
                .expect("close_issue tool"),
            &["list_issues", "update_issue"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "manage_dependencies")
                .expect("manage_dependencies tool"),
            &["list_issues", "beads://schema", "beads://issues/blocked"],
        );
        assert_tool_description_mentions(
            tools
                .iter()
                .find(|tool| tool.name == "project_overview")
                .expect("project_overview tool"),
            &[
                "beads://project/info",
                "beads://schema",
                "beads://issues/ready",
                "beads://coordination/status",
                "local SQLite/JSONL",
            ],
        );
    }

    #[test]
    fn mcp_contract_project_overview_payload_advertises_documented_resources() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(
            &state,
            "br-mcp-contract-overview",
            "overview contract issue",
        );
        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let overview = project_overview_json(&state, &storage).expect("project overview");
        let resources = overview_resource_entries(&overview);
        let overview_tool = ProjectOverviewTool::new(Arc::clone(&state)).definition();
        let description = overview_tool
            .description
            .as_deref()
            .expect("project_overview description");

        assert_eq!(overview["counts"]["total"].as_u64(), Some(1));
        assert_eq!(overview["counts"]["ready"].as_u64(), Some(1));
        assert_eq!(
            overview["ready_issues"][0]["id"].as_str(),
            Some("br-mcp-contract-overview")
        );

        for expected in [
            "beads://project/info",
            "beads://schema",
            "beads://issues/ready",
            "beads://coordination/status",
        ] {
            assert!(
                resources.iter().any(|entry| entry.starts_with(expected)),
                "project_overview discovery resources must advertise {expected}: {resources:?}"
            );
            assert!(
                description.contains(expected),
                "project_overview description must mention payload resource {expected}"
            );
            assert_docs_list_mcp_term(expected);
        }

        assert!(
            overview["discovery"]["prompts"]
                .as_array()
                .is_some_and(|prompts| prompts.iter().any(|prompt| prompt
                    .as_str()
                    .is_some_and(|text| text.starts_with("triage"))))
        );
    }

    #[test]
    fn project_overview_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        insert_test_issue(&state, "br-mcp-cache-1", "cached overview first issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ProjectOverviewTool::new(Arc::clone(&state));

        let first_content = tool
            .call(&ctx, json!({}))
            .expect("cached project overview call");
        let first = content_json(&first_content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            project_overview_json(&state, &storage).expect("direct overview")
        };
        assert_eq!(first, direct);

        insert_test_issue(&state, "br-mcp-cache-2", "cached overview second issue");
        fs::write(&state.jsonl_path, "{\"id\":\"br-mcp-cache-2\"}\n")
            .expect("update jsonl witness");

        let second_content = tool
            .call(&ctx, json!({}))
            .expect("fresh project overview after witness mismatch");
        let second = content_json(&second_content);
        assert_eq!(second["counts"]["total"].as_u64(), Some(2));
    }

    #[test]
    fn project_overview_excludes_unsatisfied_external_blockers_from_ready() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(
            &state,
            "br-mcp-external-blocked-ready",
            "externally blocked overview candidate",
        );
        insert_test_issue(&state, "br-mcp-local-ready", "local overview candidate");
        let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
        storage
            .add_dependency(
                "br-mcp-external-blocked-ready",
                "external:missing:capability",
                "blocks",
                "mcp-test",
            )
            .expect("add external dependency");
        drop(storage);

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ProjectOverviewTool::new(Arc::clone(&state));
        let content = tool.call(&ctx, json!({})).expect("project overview");
        let overview = content_json(&content);

        assert_eq!(overview["counts"]["ready"].as_u64(), Some(1));
        assert_eq!(overview["ready_issues"][0]["id"], "br-mcp-local-ready");
    }

    #[test]
    fn list_issues_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        insert_test_issue(&state, "br-mcp-list-1", "cached list first issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ListIssuesTool::new(Arc::clone(&state));
        let args = json!({"limit": 10, "sort": "created"});

        let first_content = tool
            .call(&ctx, args.clone())
            .expect("cached list_issues call");
        let first = content_json(&first_content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            list_issues_json(&storage, &args).expect("direct list")
        };
        assert_eq!(first, direct);

        insert_test_issue(&state, "br-mcp-list-2", "cached list second issue");
        fs::write(&state.jsonl_path, "{\"id\":\"br-mcp-list-2\"}\n").expect("update jsonl witness");

        let second_content = tool
            .call(&ctx, args)
            .expect("fresh list_issues after witness mismatch");
        let second = content_json(&second_content);
        assert_eq!(second["count"].as_u64(), Some(2));
    }

    #[test]
    fn list_issues_legacy_single_result_shape_is_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-list-single-1", "legacy list single first");
        insert_test_issue(&state, "br-mcp-list-single-2", "legacy list single second");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ListIssuesTool::new(state);

        let content = tool
            .call(&ctx, json!({"limit": 1, "sort": "created"}))
            .expect("legacy list_issues");
        let result = content_json(&content);

        assert_eq!(result["count"].as_u64(), Some(1));
        assert!(result["issues"].as_array().is_some());
        assert!(result.get("items").is_none());
        assert!(result.get("ok_count").is_none());
        assert!(result.get("error_count").is_none());
    }

    #[test]
    fn list_issues_batch_returns_ordered_items_partial_errors_and_coercions() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-list-batch-1", "batch list first issue");
        insert_test_issue(&state, "br-mcp-list-batch-2", "batch list second issue");
        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            storage
                .update_issue(
                    "br-mcp-list-batch-2",
                    &IssueUpdate {
                        status: Some(Status::InProgress),
                        ..IssueUpdate::default()
                    },
                    "mcp-test",
                )
                .expect("mark issue in progress");
        }
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ListIssuesTool::new(Arc::clone(&state));
        let queries = vec![
            json!({"limit": 10, "sort": "created"}),
            json!({"status": "wip", "limit": 10}),
            json!({"status": 7}),
            json!(["not", "an", "object"]),
        ];
        let args = json!({"queries": queries.clone()});

        let content = tool.call(&ctx, args).expect("batch list_issues");
        let batch = content_json(&content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            list_issues_batch_json(&storage, &queries)
        };

        assert_eq!(batch, direct);
        assert_batch_counts(&batch, 4, 2, 2);
        assert_eq!(batch["items"][0]["index"].as_u64(), Some(0));
        assert_eq!(batch["items"][0]["result"]["count"].as_u64(), Some(2));
        assert_eq!(batch["items"][1]["ok"].as_bool(), Some(true));
        assert_eq!(batch["items"][1]["result"]["count"].as_u64(), Some(1));
        assert!(
            batch["items"][1]["result"]["coercions"]
                .as_array()
                .is_some_and(|coercions| coercions
                    .iter()
                    .any(|warning| warning.as_str().is_some_and(|s| s.contains("wip"))))
        );
        assert_eq!(batch["items"][2]["ok"].as_bool(), Some(false));
        assert_eq!(
            batch["items"][2]["error"]["kind"].as_str(),
            Some("InvalidParams")
        );
        assert_eq!(
            batch["items"][3]["error"]["message"].as_str(),
            Some("'queries[3]' must be a filter object, got [\"not\",\"an\",\"object\"]")
        );
    }

    #[test]
    fn list_issues_batch_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ListIssuesTool::new(state);

        let err = tool
            .call(&ctx, json!({"status": "open", "queries": [{}]}))
            .expect_err("single filters and queries together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either list filters"));
    }

    #[test]
    fn list_issues_batch_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        insert_test_issue(
            &state,
            "br-mcp-list-batch-cache-1",
            "cached batch list first",
        );
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ListIssuesTool::new(Arc::clone(&state));
        let queries = vec![
            json!({"limit": 10, "sort": "created"}),
            json!({"title": "cached batch list", "limit": 10}),
        ];
        let args = json!({"queries": queries.clone()});

        let first = content_json(&tool.call(&ctx, args.clone()).expect("cached batch"));
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            list_issues_batch_json(&storage, &queries)
        };
        assert_eq!(first, direct);

        insert_test_issue(
            &state,
            "br-mcp-list-batch-cache-2",
            "cached batch list second",
        );
        fs::write(
            &state.jsonl_path,
            "{\"id\":\"br-mcp-list-batch-cache-2\"}\n",
        )
        .expect("update jsonl witness");

        let second = content_json(&tool.call(&ctx, args).expect("fresh batch"));
        assert_eq!(second["items"][0]["result"]["count"].as_u64(), Some(2));
        assert_eq!(second["items"][1]["result"]["count"].as_u64(), Some(2));
    }

    #[test]
    fn show_issue_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        let id = "br-mcp-show-1";
        insert_test_issue(&state, id, "cached show first title");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ShowIssueTool::new(Arc::clone(&state));
        let args = json!({"id": id});

        let first_content = tool
            .call(&ctx, args.clone())
            .expect("cached show_issue call");
        let first = content_json(&first_content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            show_issue_json(&storage, id).expect("direct show")
        };
        assert_eq!(first, direct);
        assert!(first["next_actions"].as_array().is_some_and(|actions| {
            actions.iter().any(|action| {
                action.as_str() == Some("No assignee — consider assigning with update_issue.")
            })
        }));

        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            let update = IssueUpdate {
                title: Some("cached show updated title".to_string()),
                ..IssueUpdate::default()
            };
            storage
                .update_issue(id, &update, "mcp-test")
                .expect("update issue title");
        }
        fs::write(&state.jsonl_path, "{\"id\":\"br-mcp-show-1\"}\n").expect("update jsonl witness");

        let second_content = tool
            .call(&ctx, args)
            .expect("fresh show_issue after witness mismatch");
        let second = content_json(&second_content);
        assert_eq!(second["title"].as_str(), Some("cached show updated title"));
    }

    #[test]
    fn show_issue_batch_returns_ordered_items_and_per_item_errors() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-batch-1", "batch first issue");
        insert_test_issue(&state, "br-mcp-batch-2", "batch second issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ShowIssueTool::new(Arc::clone(&state));
        let args = json!({"ids": ["br-mcp-batch-1", "missing-id", "YOUR_ID", "br-mcp-batch-2"]});

        let content = tool.call(&ctx, args.clone()).expect("batch show_issue");
        let batch = content_json(&content);
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            show_issue_batch_json(
                &storage,
                &[
                    "br-mcp-batch-1".to_string(),
                    "missing-id".to_string(),
                    "YOUR_ID".to_string(),
                    "br-mcp-batch-2".to_string(),
                ],
            )
        };

        assert_eq!(batch, direct);
        assert_eq!(batch["count"].as_u64(), Some(4));
        assert_eq!(batch["ok_count"].as_u64(), Some(2));
        assert_eq!(batch["error_count"].as_u64(), Some(2));
        assert_eq!(batch["items"][0]["id"].as_str(), Some("br-mcp-batch-1"));
        assert_eq!(
            batch["items"][1]["error"]["data"]["error_type"].as_str(),
            Some("ISSUE_NOT_FOUND")
        );
        assert_eq!(
            batch["items"][2]["error"]["data"]["error_type"].as_str(),
            Some("PLACEHOLDER_DETECTED")
        );
    }

    #[test]
    fn show_issue_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ShowIssueTool::new(state);

        let err = tool
            .call(&ctx, json!({"id": "br-one", "ids": ["br-two"]}))
            .expect_err("id and ids together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either 'id'"));
    }

    #[test]
    fn show_issue_batch_snapshot_matches_direct_json_and_invalidates() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        insert_test_issue(&state, "br-mcp-batch-cache-1", "cached batch first title");
        insert_test_issue(&state, "br-mcp-batch-cache-2", "cached batch second title");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ShowIssueTool::new(Arc::clone(&state));
        let ids = vec![
            "br-mcp-batch-cache-1".to_string(),
            "br-mcp-batch-cache-2".to_string(),
        ];
        let args = json!({"ids": ids});

        let first = content_json(&tool.call(&ctx, args.clone()).expect("cached batch"));
        let direct = {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            show_issue_batch_json(&storage, &ids)
        };
        assert_eq!(first, direct);

        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            storage
                .update_issue(
                    "br-mcp-batch-cache-2",
                    &IssueUpdate {
                        title: Some("cached batch updated title".to_string()),
                        ..IssueUpdate::default()
                    },
                    "mcp-test",
                )
                .expect("update issue title");
        }
        fs::write(&state.jsonl_path, "{\"id\":\"br-mcp-batch-cache-2\"}\n")
            .expect("update jsonl witness");

        let second = content_json(&tool.call(&ctx, args).expect("fresh batch"));
        assert_eq!(
            second["items"][1]["issue"]["title"].as_str(),
            Some("cached batch updated title")
        );
    }

    #[test]
    fn create_issue_legacy_single_result_shape_is_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CreateIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "title": "single create issue",
                    "type": "feat",
                    "priority": "urgent",
                    "labels": ["team:create"]
                }),
            )
            .expect("single create");
        let result = content_json(&content);

        assert_eq!(result["title"].as_str(), Some("single create issue"));
        assert_eq!(result["status"].as_str(), Some("open"));
        assert_eq!(result["type"].as_str(), Some("feature"));
        assert!(result["id"].as_str().is_some());
        assert!(result.get("items").is_none());
        assert!(result.get("count").is_none());

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let id = result["id"].as_str().expect("created id");
        assert_eq!(
            storage.get_labels(id).expect("labels"),
            vec!["team:create".to_string()]
        );
    }

    #[test]
    fn create_issue_batch_returns_ordered_items_partial_errors_and_flushes() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-create-parent", "batch parent issue");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CreateIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "issues": [
                        {
                            "title": "batch create first",
                            "type": "feat",
                            "priority": "urgent",
                            "labels": ["team:create"]
                        },
                        {"title": ""},
                        {
                            "title": "batch create child",
                            "parent": "br-mcp-create-parent",
                            "labels": ["team:child"]
                        },
                        {
                            "title": "batch create missing parent",
                            "parent": "missing-parent"
                        }
                    ]
                }),
            )
            .expect("batch create");
        let batch = content_json(&content);

        assert_batch_counts(&batch, 4, 2, 2);
        assert_eq!(batch["items"][0]["index"].as_u64(), Some(0));
        assert_eq!(batch["items"][0]["ok"].as_bool(), Some(true));
        assert_eq!(
            batch["items"][0]["result"]["type"].as_str(),
            Some("feature")
        );
        assert_eq!(batch["items"][1]["ok"].as_bool(), Some(false));
        assert!(
            batch["items"][1]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Title"))
        );
        assert_eq!(
            batch["items"][2]["result"]["parent"].as_str(),
            Some("br-mcp-create-parent")
        );
        assert_eq!(
            batch["items"][3]["error"]["data"]["error_type"].as_str(),
            Some("ISSUE_NOT_FOUND")
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let first_id = batch["items"][0]["id"].as_str().expect("first id");
        let child_id = batch["items"][2]["id"].as_str().expect("child id");
        assert_eq!(
            storage.get_labels(first_id).expect("first labels"),
            vec!["team:create".to_string()]
        );
        assert_eq!(
            storage.get_labels(child_id).expect("child labels"),
            vec!["team:child".to_string()]
        );
        let child_deps = storage
            .get_dependencies_full(child_id)
            .expect("child dependencies");
        assert!(child_deps.iter().any(|dep| {
            dep.depends_on_id == "br-mcp-create-parent"
                && dep.dep_type == DependencyType::ParentChild
        }));

        let jsonl = fs::read_to_string(&state.jsonl_path).expect("read auto-flushed jsonl");
        assert!(jsonl.contains("batch create first"));
        assert!(jsonl.contains("batch create child"));
        assert!(!jsonl.contains("batch create missing parent"));
    }

    #[test]
    fn create_issue_batch_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CreateIssueTool::new(state);

        let err = tool
            .call(
                &ctx,
                json!({
                    "title": "single title",
                    "issues": [{"title": "batch title"}]
                }),
            )
            .expect_err("title and issues together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either title"));
    }

    #[test]
    #[ignore = "perf probe for MCP create_issue batch evidence"]
    fn mcp_create_issue_batch_perf_probe() {
        let issue_count = 25_usize;
        let iterations = 5_usize;
        let ctx = McpContext::new(Cx::for_testing(), 1);

        let single_temp = TempDir::new().expect("single tempdir");
        let single_state = mcp_test_state(&single_temp);
        let single_tool = CreateIssueTool::new(Arc::clone(&single_state));
        let repeated_started = Instant::now();
        for round in 0..iterations {
            for index in 0..issue_count {
                single_tool
                    .call(
                        &ctx,
                        json!({"title": format!("single create round {round} issue {index:04}")}),
                    )
                    .expect("single create");
            }
        }
        let repeated = repeated_started.elapsed();

        let batch_temp = TempDir::new().expect("batch tempdir");
        let batch_state = mcp_test_state(&batch_temp);
        let batch_tool = CreateIssueTool::new(Arc::clone(&batch_state));
        let batch_started = Instant::now();
        let mut last_batch = serde_json::Value::Null;
        for round in 0..iterations {
            let issues = (0..issue_count)
                .map(|index| json!({"title": format!("batch create round {round} issue {index:04}")}))
                .collect::<Vec<_>>();
            let content = batch_tool
                .call(&ctx, json!({"issues": issues}))
                .expect("batch create");
            last_batch = content_json(&content);
            let issue_count_u64 = u64::try_from(issue_count).expect("issue count fits u64");
            assert_batch_counts(&last_batch, issue_count_u64, issue_count_u64, 0);
        }
        let batch = batch_started.elapsed();

        let expected_total = issue_count
            .checked_mul(iterations)
            .expect("expected total issue count fits usize");
        let storage = SqliteStorage::open(&batch_state.db_path).expect("open batch storage");
        assert_eq!(
            storage.count_all_issues().expect("count batch issues"),
            expected_total
        );

        println!(
            "{}",
            json!({
                "issues": issue_count,
                "iterations": iterations,
                "repeated_single_total_ns": repeated.as_nanos(),
                "batch_total_ns": batch.as_nanos(),
                "speedup": repeated.as_secs_f64() / batch.as_secs_f64(),
                "last_batch_ok_count": last_batch["ok_count"],
                "equality": "batch issue creates verified by final storage count and per-item ok counts"
            })
        );
    }

    #[test]
    fn update_issue_legacy_single_result_shape_is_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-update-single", "single update original");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = UpdateIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "id": "br-mcp-update-single",
                    "title": "single update changed",
                }),
            )
            .expect("single update");
        let result = content_json(&content);

        assert_eq!(result["id"].as_str(), Some("br-mcp-update-single"));
        assert_eq!(result["title"].as_str(), Some("single update changed"));
        assert!(result.get("items").is_none());
        assert!(result.get("count").is_none());
    }

    #[test]
    fn update_issue_edits_acceptance_checklist_in_place() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-acceptance", "acceptance checklist");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = UpdateIssueTool::new(Arc::clone(&state));
        let call = |args: Value| tool.call(&ctx, args).map(|content| content_json(&content));

        let added = call(json!({
            "id": "br-mcp-acceptance",
            "add_acceptance": ["first criterion", "second criterion"],
        }))
        .expect("append items");
        assert_eq!(added["acceptance_criteria"]["total"], 2, "{added}");
        assert_eq!(added["acceptance_criteria"]["changed"], true, "{added}");
        assert_eq!(
            added["acceptance_criteria"]["added"],
            json!([1, 2]),
            "{added}"
        );

        let ticked = call(json!({"id": "br-mcp-acceptance", "check_acceptance": ["second"]}))
            .expect("tick by text");
        let items = &ticked["acceptance_criteria"]["items"];
        assert_eq!(items[0]["checked"], false, "{ticked}");
        assert_eq!(items[1]["checked"], true, "{ticked}");
        assert_eq!(ticked["acceptance_criteria"]["remaining"], 1, "{ticked}");

        // Re-ticking is idempotent: reported, but the field is not rewritten.
        let again = call(json!({"id": "br-mcp-acceptance", "check_acceptance": ["2"]}))
            .expect("idempotent tick");
        assert_eq!(again["acceptance_criteria"]["changed"], false, "{again}");

        // Bad selectors are rejected before any write, with the argument named.
        let err = call(json!({"id": "br-mcp-acceptance", "check_acceptance": ["9"]}))
            .expect_err("out-of-range index should fail");
        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(
            err.message.starts_with("check-acceptance:"),
            "{}",
            err.message
        );
        assert!(err.message.contains("does not exist"), "{}", err.message);

        let stored = SqliteStorage::open(&state.db_path)
            .expect("open storage")
            .get_issue("br-mcp-acceptance")
            .expect("get issue")
            .expect("issue exists");
        assert_eq!(
            stored.acceptance_criteria.as_deref(),
            Some("- [ ] first criterion\n- [x] second criterion")
        );

        // show_issue exposes the same structured view.
        let shown = show_issue_json(
            &SqliteStorage::open(&state.db_path).expect("open storage"),
            "br-mcp-acceptance",
        )
        .expect("show issue");
        assert_eq!(
            shown["acceptance_items"][1]["text"], "second criterion",
            "{shown}"
        );
        assert_eq!(shown["acceptance_items"][1]["checked"], true, "{shown}");
    }

    #[test]
    fn update_issue_batch_returns_ordered_items_partial_errors_and_flushes() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-update-batch-1", "batch first original");
        insert_test_issue(&state, "br-mcp-update-batch-2", "batch second original");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = UpdateIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "updates": [
                        {"id": "br-mcp-update-batch-1", "title": "batch first changed"},
                        {"id": "missing-id", "title": "missing should fail"},
                        {"id": "YOUR_ID", "title": "placeholder should fail"},
                        {
                            "id": "br-mcp-update-batch-2",
                            "status": "wip",
                            "labels_add": ["team:batch"],
                            "comment": "batch comment"
                        }
                    ]
                }),
            )
            .expect("batch update");
        let batch = content_json(&content);

        assert_batch_counts(&batch, 4, 2, 2);
        assert_eq!(batch["items"][0]["index"].as_u64(), Some(0));
        assert_eq!(batch["items"][0]["ok"].as_bool(), Some(true));
        assert_eq!(
            batch["items"][0]["result"]["title"].as_str(),
            Some("batch first changed")
        );
        assert_eq!(
            batch["items"][1]["error"]["data"]["error_type"].as_str(),
            Some("ISSUE_NOT_FOUND")
        );
        assert_eq!(
            batch["items"][2]["error"]["data"]["error_type"].as_str(),
            Some("PLACEHOLDER_DETECTED")
        );
        assert_eq!(
            batch["items"][3]["result"]["status"].as_str(),
            Some("in_progress")
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let first = storage
            .get_issue("br-mcp-update-batch-1")
            .expect("get first issue")
            .expect("first issue exists");
        let second = storage
            .get_issue("br-mcp-update-batch-2")
            .expect("get second issue")
            .expect("second issue exists");
        assert_eq!(first.title, "batch first changed");
        assert_eq!(second.status, Status::InProgress);
        assert_eq!(
            storage.get_labels("br-mcp-update-batch-2").expect("labels"),
            vec!["team:batch".to_string()]
        );
        let comments = storage
            .get_comments("br-mcp-update-batch-2")
            .expect("comments");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "mcp-test");
        assert_eq!(comments[0].body, "batch comment");

        let jsonl = fs::read_to_string(&state.jsonl_path).expect("read auto-flushed jsonl");
        assert!(jsonl.contains("batch first changed"));
        assert!(jsonl.contains("batch comment"));
    }

    #[test]
    fn update_issue_batch_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = UpdateIssueTool::new(state);

        let err = tool
            .call(
                &ctx,
                json!({
                    "id": "br-one",
                    "updates": [{"id": "br-two", "title": "two"}]
                }),
            )
            .expect_err("id and updates together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either 'id'"));
    }

    #[test]
    fn update_issue_batch_rejects_invalid_comment_before_item_field_mutation() {
        // Per-item transactional pre-flight: an invalid comment in a batch
        // update must reject that item without applying its field mutation.
        // Comment size is no longer a validation axis (long-form bodies are
        // explicitly supported); use NUL-byte poison to stay invariant.
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-update-invalid-comment", "comment original");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = UpdateIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "updates": [{
                        "id": "br-mcp-update-invalid-comment",
                        "title": "should not mutate",
                        "comment": "valid prefix\0NUL byte poisons this comment"
                    }]
                }),
            )
            .expect("batch update should return per-item error");
        let batch = content_json(&content);

        assert_batch_counts(&batch, 1, 0, 1);
        assert_eq!(batch["items"][0]["ok"].as_bool(), Some(false));
        assert!(
            batch["items"][0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("content") && message.contains("NUL"))
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let issue = storage
            .get_issue("br-mcp-update-invalid-comment")
            .expect("get issue")
            .expect("issue exists");
        assert_eq!(issue.title, "comment original");
        assert!(
            storage
                .get_comments("br-mcp-update-invalid-comment")
                .expect("comments")
                .is_empty(),
            "invalid batch comment must not be inserted"
        );
    }

    #[test]
    fn close_issue_legacy_single_result_shape_is_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-close-single", "single close original");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CloseIssueTool::new(Arc::clone(&state));

        let content = tool
            .call(
                &ctx,
                json!({
                    "id": "br-mcp-close-single",
                    "reason": "single close done",
                }),
            )
            .expect("single close");
        let result = content_json(&content);

        assert_eq!(result["id"].as_str(), Some("br-mcp-close-single"));
        assert_eq!(result["status"].as_str(), Some("closed"));
        assert_eq!(result["close_reason"].as_str(), Some("single close done"));
        assert!(result.get("items").is_none());
        assert!(result.get("count").is_none());

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let issue = storage
            .get_issue("br-mcp-close-single")
            .expect("get issue")
            .expect("issue exists");
        assert_eq!(issue.status, Status::Closed);
    }

    #[test]
    fn close_issue_batch_returns_ordered_items_partial_errors_and_flushes() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-close-batch-1", "batch first close");
        insert_test_issue(&state, "br-mcp-close-batch-2", "batch second close");
        insert_test_issue(
            &state,
            "br-mcp-close-dependent",
            "dependent unblocked by second close",
        );
        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            storage
                .add_dependency(
                    "br-mcp-close-dependent",
                    "br-mcp-close-batch-2",
                    "blocks",
                    "mcp-test",
                )
                .expect("add dependency");
        }

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CloseIssueTool::new(Arc::clone(&state));
        let content = tool
            .call(
                &ctx,
                json!({
                    "ids": [
                        "br-mcp-close-batch-1",
                        "missing-id",
                        "YOUR_ID",
                        "br-mcp-close-batch-2"
                    ],
                    "reason": "batch close done",
                }),
            )
            .expect("batch close");
        let batch = content_json(&content);

        assert_batch_counts(&batch, 4, 2, 2);
        assert_eq!(batch["items"][0]["index"].as_u64(), Some(0));
        assert_eq!(batch["items"][0]["ok"].as_bool(), Some(true));
        assert_eq!(
            batch["items"][0]["result"]["status"].as_str(),
            Some("closed")
        );
        assert_eq!(
            batch["items"][1]["error"]["data"]["error_type"].as_str(),
            Some("ISSUE_NOT_FOUND")
        );
        assert_eq!(
            batch["items"][2]["error"]["data"]["error_type"].as_str(),
            Some("PLACEHOLDER_DETECTED")
        );
        assert!(
            batch["items"][3]["result"]["unblocked_candidates"]
                .as_array()
                .is_some_and(|items| items
                    .iter()
                    .any(|id| id.as_str() == Some("br-mcp-close-dependent")))
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        for id in ["br-mcp-close-batch-1", "br-mcp-close-batch-2"] {
            let issue = storage
                .get_issue(id)
                .expect("get issue")
                .expect("issue exists");
            assert_eq!(issue.status, Status::Closed);
            assert_eq!(issue.close_reason.as_deref(), Some("batch close done"));
        }
        let missing = storage.get_issue("missing-id").expect("get missing");
        assert!(missing.is_none());

        let jsonl = fs::read_to_string(&state.jsonl_path).expect("read auto-flushed jsonl");
        assert!(jsonl.contains("batch close done"));
        assert!(jsonl.contains("br-mcp-close-batch-1"));
        assert!(jsonl.contains("br-mcp-close-batch-2"));
    }

    #[test]
    fn close_issue_batch_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = CloseIssueTool::new(state);

        let err = tool
            .call(
                &ctx,
                json!({
                    "id": "br-one",
                    "ids": ["br-two"],
                }),
            )
            .expect_err("id and ids together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either 'id'"));
    }

    fn time_repeated_json_reads<F>(
        iterations: u32,
        mut read: F,
    ) -> (std::time::Duration, serde_json::Value)
    where
        F: FnMut() -> serde_json::Value,
    {
        let started = Instant::now();
        let mut last = serde_json::Value::Null;
        for _ in 0..iterations {
            last = read();
        }
        (started.elapsed(), last)
    }

    #[test]
    #[ignore = "perf probe for MCP list_issues batch evidence"]
    fn mcp_list_issues_batch_perf_probe() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        for index in 0..150 {
            insert_test_issue(
                &state,
                &format!("br-mcp-list-batch-perf-{index:04}"),
                &format!("MCP list batch perf issue {index:04}"),
            );
        }

        let queries: Vec<Value> = (0..25)
            .map(|index| match index % 5 {
                0 => json!({"limit": 20, "sort": "created"}),
                1 => json!({"status": "open", "limit": 20}),
                2 => json!({"priority": "normal", "limit": 20}),
                3 => json!({"title": format!("perf issue {:02}", index / 5), "limit": 20}),
                _ => json!({"search": "perf", "limit": 20}),
            })
            .collect();

        let iterations = 5_u32;
        let repeated_single = time_repeated_json_reads(iterations, || {
            let mut items = Vec::with_capacity(queries.len());
            for (index, query) in queries.iter().enumerate() {
                let storage = SqliteStorage::open(&state.db_path).expect("open storage");
                let result = list_issues_json(&storage, query).expect("single list query");
                items.push(json!({
                    "index": index,
                    "query": query,
                    "ok": true,
                    "result": result,
                }));
            }
            json!({
                "items": items,
                "count": queries.len(),
                "ok_count": queries.len(),
                "error_count": 0,
            })
        });
        let batch = time_repeated_json_reads(iterations, || {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            list_issues_batch_json(&storage, &queries)
        });

        assert_eq!(batch.1, repeated_single.1);
        println!(
            "{}",
            json!({
                "queries": queries.len(),
                "seeded_issues": 150,
                "iterations": iterations,
                "repeated_single_total_ns": repeated_single.0.as_nanos(),
                "batch_total_ns": batch.0.as_nanos(),
                "speedup": repeated_single.0.as_nanos() as f64
                    / batch.0.as_nanos().max(1) as f64,
                "equality": "batch envelope matches repeated single-query list_issues JSON",
            })
        );
    }

    fn dependency_perf_rounds(
        prefix: &str,
        issue_count: usize,
        iterations: usize,
    ) -> Vec<(String, Vec<String>)> {
        (0..iterations)
            .map(|round| {
                let blocker = format!("br-mcp-dep-{prefix}-perf-blocker-{round}");
                let ids = (0..issue_count)
                    .map(|index| format!("br-mcp-dep-{prefix}-perf-{round}-{index:04}"))
                    .collect::<Vec<_>>();
                (blocker, ids)
            })
            .collect()
    }

    fn seed_dependency_perf_rounds(state: &BeadsState, rounds: &[(String, Vec<String>)]) {
        for (blocker, ids) in rounds {
            insert_test_issue(
                state,
                blocker,
                &format!("Dependency perf blocker {blocker}"),
            );
            for id in ids {
                insert_test_issue(state, id, &format!("Dependency perf issue {id}"));
            }
        }
    }

    fn assert_dependency_perf_rounds(state: &BeadsState, rounds: &[(String, Vec<String>)]) {
        let storage = SqliteStorage::open(&state.db_path).expect("open batch storage");
        for (blocker, ids) in rounds {
            for id in ids {
                let deps = storage
                    .get_dependencies_full(id)
                    .expect("load batch dependency");
                assert!(deps.iter().any(|dep| dep.depends_on_id == *blocker));
            }
        }
    }

    #[test]
    #[ignore = "perf probe for MCP read snapshot evidence"]
    fn mcp_read_snapshot_perf_probe() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state_with_read_snapshot(&temp, true);
        for index in 0..250 {
            insert_test_issue(
                &state,
                &format!("br-mcp-perf-{index:04}"),
                &format!("MCP perf issue {index:04}"),
            );
        }

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let overview_tool = ProjectOverviewTool::new(Arc::clone(&state));
        let list_tool = ListIssuesTool::new(Arc::clone(&state));
        let show_tool = ShowIssueTool::new(Arc::clone(&state));
        let list_args = json!({"limit": 250, "sort": "created"});
        let show_args = json!({"id": "br-mcp-perf-0000"});
        let iterations = 250_u32;

        let direct_overview = time_repeated_json_reads(iterations, || {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            project_overview_json(&state, &storage).expect("direct overview")
        });

        let first_cached_overview = overview_tool
            .call(&ctx, json!({}))
            .expect("warm overview snapshot");
        assert_eq!(content_json(&first_cached_overview), direct_overview.1);

        let cached_overview = time_repeated_json_reads(iterations, || {
            let content = overview_tool
                .call(&ctx, json!({}))
                .expect("cached overview call");
            content_json(&content)
        });
        assert_eq!(cached_overview.1, direct_overview.1);

        let direct_list = time_repeated_json_reads(iterations, || {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            list_issues_json(&storage, &list_args).expect("direct list")
        });

        let first_cached_list = list_tool
            .call(&ctx, list_args.clone())
            .expect("warm list snapshot");
        assert_eq!(content_json(&first_cached_list), direct_list.1);

        let cached_list = time_repeated_json_reads(iterations, || {
            let content = list_tool
                .call(&ctx, list_args.clone())
                .expect("cached list call");
            content_json(&content)
        });
        assert_eq!(cached_list.1, direct_list.1);

        let direct_show = time_repeated_json_reads(iterations, || {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            show_issue_json(&storage, "br-mcp-perf-0000").expect("direct show")
        });

        let first_cached_show = show_tool
            .call(&ctx, show_args.clone())
            .expect("warm show snapshot");
        assert_eq!(content_json(&first_cached_show), direct_show.1);

        let cached_show = time_repeated_json_reads(iterations, || {
            let content = show_tool
                .call(&ctx, show_args.clone())
                .expect("cached show call");
            content_json(&content)
        });
        assert_eq!(cached_show.1, direct_show.1);

        let direct_overview_ns = direct_overview.0.as_nanos();
        let cached_overview_ns = cached_overview.0.as_nanos();
        let direct_list_ns = direct_list.0.as_nanos();
        let cached_list_ns = cached_list.0.as_nanos();
        let direct_show_ns = direct_show.0.as_nanos();
        let cached_show_ns = cached_show.0.as_nanos();

        println!(
            "{}",
            json!({
                "issues": 250,
                "iterations": iterations,
                "project_overview": {
                    "direct_total_ns": direct_overview_ns,
                    "cached_total_ns": cached_overview_ns,
                    "speedup": direct_overview_ns as f64 / cached_overview_ns.max(1) as f64,
                },
                "list_issues": {
                    "direct_total_ns": direct_list_ns,
                    "cached_total_ns": cached_list_ns,
                    "speedup": direct_list_ns as f64 / cached_list_ns.max(1) as f64,
                },
                "show_issue": {
                    "direct_total_ns": direct_show_ns,
                    "cached_total_ns": cached_show_ns,
                    "speedup": direct_show_ns as f64 / cached_show_ns.max(1) as f64,
                },
            })
        );
    }

    #[test]
    #[ignore = "perf probe for MCP show_issue batch evidence"]
    fn mcp_show_issue_batch_perf_probe() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ids: Vec<String> = (0..25)
            .map(|index| format!("br-mcp-batch-perf-{index:04}"))
            .collect();
        for id in &ids {
            insert_test_issue(&state, id, &format!("MCP batch perf issue {id}"));
        }

        let iterations = 10_u32;
        let repeated_single = time_repeated_json_reads(iterations, || {
            let mut items = Vec::with_capacity(ids.len());
            for id in &ids {
                let storage = SqliteStorage::open(&state.db_path).expect("open storage");
                let issue = show_issue_json(&storage, id).expect("single show");
                items.push(json!({"id": id, "ok": true, "issue": issue}));
            }
            json!({
                "items": items,
                "count": ids.len(),
                "ok_count": ids.len(),
                "error_count": 0,
            })
        });
        let batch = time_repeated_json_reads(iterations, || {
            let storage = SqliteStorage::open(&state.db_path).expect("open storage");
            show_issue_batch_json(&storage, &ids)
        });

        assert_eq!(batch.1, repeated_single.1);
        println!(
            "{}",
            json!({
                "issues": ids.len(),
                "iterations": iterations,
                "repeated_single_total_ns": repeated_single.0.as_nanos(),
                "batch_total_ns": batch.0.as_nanos(),
                "speedup": repeated_single.0.as_nanos() as f64
                    / batch.0.as_nanos().max(1) as f64,
                "equality": "batch envelope matches repeated single-item issue JSON",
            })
        );
    }

    #[test]
    #[ignore = "perf probe for MCP update_issue batch evidence"]
    fn mcp_update_issue_batch_perf_probe() {
        let ids = (0..25)
            .map(|index| format!("br-mcp-update-batch-perf-{index:04}"))
            .collect::<Vec<_>>();
        let iterations = 5_u32;

        let repeated_temp = TempDir::new().expect("repeated tempdir");
        let repeated_state = mcp_test_state(&repeated_temp);
        for id in &ids {
            insert_test_issue(&repeated_state, id, &format!("Repeated update issue {id}"));
        }
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let repeated_tool = UpdateIssueTool::new(Arc::clone(&repeated_state));
        let repeated_started = Instant::now();
        for round in 0..iterations {
            for id in &ids {
                repeated_tool
                    .call(
                        &ctx,
                        json!({
                            "id": id,
                            "title": format!("repeated round {round} {id}")
                        }),
                    )
                    .expect("single update");
            }
        }
        let repeated = repeated_started.elapsed();

        let batch_temp = TempDir::new().expect("batch tempdir");
        let batch_state = mcp_test_state(&batch_temp);
        for id in &ids {
            insert_test_issue(&batch_state, id, &format!("Batch update issue {id}"));
        }
        let batch_tool = UpdateIssueTool::new(Arc::clone(&batch_state));
        let batch_started = Instant::now();
        let mut last_batch = serde_json::Value::Null;
        for round in 0..iterations {
            let updates = ids
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "title": format!("batch round {round} {id}")
                    })
                })
                .collect::<Vec<_>>();
            let content = batch_tool
                .call(&ctx, json!({"updates": updates}))
                .expect("batch update");
            last_batch = content_json(&content);
            let ids_len = u64::try_from(ids.len()).expect("ids len fits in u64");
            assert_batch_counts(&last_batch, ids_len, ids_len, 0);
        }
        let batch = batch_started.elapsed();

        let storage = SqliteStorage::open(&batch_state.db_path).expect("open batch storage");
        for id in &ids {
            let issue = storage
                .get_issue(id)
                .expect("get issue")
                .expect("issue exists");
            assert!(issue.title.starts_with("batch round 4 "));
        }

        println!(
            "{}",
            json!({
                "issues": ids.len(),
                "iterations": iterations,
                "repeated_single_total_ns": repeated.as_nanos(),
                "batch_total_ns": batch.as_nanos(),
                "speedup": repeated.as_nanos() as f64 / batch.as_nanos().max(1) as f64,
                "last_batch_ok_count": last_batch["ok_count"],
                "equality": "batch updates verified by final storage state and per-item ok counts",
            })
        );
    }

    #[test]
    #[ignore = "perf probe for MCP close_issue batch evidence"]
    fn mcp_close_issue_batch_perf_probe() {
        let issue_count = 25;
        let iterations = 5;
        let repeated_temp = TempDir::new().expect("repeated tempdir");
        let repeated_state = mcp_test_state(&repeated_temp);
        let repeated_ids = (0..iterations)
            .map(|round| {
                (0..issue_count)
                    .map(|index| format!("br-mcp-close-single-perf-{round}-{index:04}"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for ids in &repeated_ids {
            for id in ids {
                insert_test_issue(&repeated_state, id, &format!("Single close issue {id}"));
            }
        }

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let repeated_tool = CloseIssueTool::new(Arc::clone(&repeated_state));
        let repeated_started = Instant::now();
        for ids in &repeated_ids {
            for id in ids {
                repeated_tool
                    .call(&ctx, json!({"id": id, "reason": "perf close done"}))
                    .expect("single close");
            }
        }
        let repeated = repeated_started.elapsed();

        let batch_temp = TempDir::new().expect("batch tempdir");
        let batch_state = mcp_test_state(&batch_temp);
        let batch_ids = (0..iterations)
            .map(|round| {
                (0..issue_count)
                    .map(|index| format!("br-mcp-close-batch-perf-{round}-{index:04}"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for ids in &batch_ids {
            for id in ids {
                insert_test_issue(&batch_state, id, &format!("Batch close issue {id}"));
            }
        }

        let batch_tool = CloseIssueTool::new(Arc::clone(&batch_state));
        let batch_started = Instant::now();
        let mut last_batch = serde_json::Value::Null;
        for ids in &batch_ids {
            let content = batch_tool
                .call(&ctx, json!({"ids": ids, "reason": "perf close done"}))
                .expect("batch close");
            last_batch = content_json(&content);
            let len = u64::try_from(ids.len()).expect("ids length fits u64");
            assert_batch_counts(&last_batch, len, len, 0);
        }
        let batch = batch_started.elapsed();

        let storage = SqliteStorage::open(&batch_state.db_path).expect("open batch storage");
        for ids in &batch_ids {
            for id in ids {
                let issue = storage
                    .get_issue(id)
                    .expect("get issue")
                    .expect("issue exists");
                assert_eq!(issue.status, Status::Closed);
                assert_eq!(issue.close_reason.as_deref(), Some("perf close done"));
            }
        }

        println!(
            "{}",
            json!({
                "issues": issue_count,
                "iterations": iterations,
                "repeated_single_total_ns": repeated.as_nanos(),
                "batch_total_ns": batch.as_nanos(),
                "speedup": repeated.as_nanos() as f64 / batch.as_nanos().max(1) as f64,
                "last_batch_ok_count": last_batch["ok_count"],
                "equality": "batch closes verified by final storage state and per-item ok counts",
            })
        );
    }

    #[test]
    #[ignore = "perf probe for MCP manage_dependencies batch evidence"]
    fn mcp_manage_dependencies_batch_perf_probe() {
        let issue_count = 25_usize;
        let iterations = 5_usize;

        let repeated_temp = TempDir::new().expect("repeated tempdir");
        let repeated_state = mcp_test_state(&repeated_temp);
        let repeated_rounds = dependency_perf_rounds("single", issue_count, iterations);
        seed_dependency_perf_rounds(&repeated_state, &repeated_rounds);

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let repeated_tool = ManageDependenciesTool::new(Arc::clone(&repeated_state));
        let repeated_started = Instant::now();
        for (blocker, ids) in &repeated_rounds {
            for id in ids {
                repeated_tool
                    .call(
                        &ctx,
                        json!({
                            "action": "add",
                            "id": id,
                            "depends_on": blocker,
                        }),
                    )
                    .expect("single dependency add");
            }
        }
        let repeated = repeated_started.elapsed();

        let batch_temp = TempDir::new().expect("batch tempdir");
        let batch_state = mcp_test_state(&batch_temp);
        let batch_rounds = dependency_perf_rounds("batch", issue_count, iterations);
        seed_dependency_perf_rounds(&batch_state, &batch_rounds);

        let batch_tool = ManageDependenciesTool::new(Arc::clone(&batch_state));
        let batch_started = Instant::now();
        let mut last_batch = serde_json::Value::Null;
        let expected_count = u64::try_from(issue_count).expect("issue count fits u64");
        for (blocker, ids) in &batch_rounds {
            let operations = ids
                .iter()
                .map(|id| {
                    json!({
                        "action": "add",
                        "id": id,
                        "depends_on": blocker,
                    })
                })
                .collect::<Vec<_>>();
            let content = batch_tool
                .call(&ctx, json!({"operations": operations}))
                .expect("batch dependency add");
            last_batch = content_json(&content);
            assert_batch_counts(&last_batch, expected_count, expected_count, 0);
        }
        let batch = batch_started.elapsed();

        assert_dependency_perf_rounds(&batch_state, &batch_rounds);

        println!(
            "{}",
            json!({
                "issues": issue_count,
                "iterations": iterations,
                "repeated_single_total_ns": repeated.as_nanos(),
                "batch_total_ns": batch.as_nanos(),
                "speedup": repeated.as_nanos() as f64 / batch.as_nanos().max(1) as f64,
                "last_batch_ok_count": last_batch["ok_count"],
                "equality": "batch dependency adds verified by final storage state and per-item ok counts",
            })
        );
    }

    #[test]
    fn parse_update_fields_rejects_non_string_priority() {
        let err = parse_update_fields("beads_rust-1234", &json!({"priority": 0}))
            .expect_err("numeric priority must be rejected");

        assert!(err.to_string().contains("'priority' must be a string"));
    }

    #[test]
    fn parse_update_fields_rejects_non_string_title() {
        let err = parse_update_fields("beads_rust-1234", &json!({"title": ["bad"]}))
            .expect_err("array title must be rejected");

        assert!(err.to_string().contains("'title' must be a string"));
    }

    #[test]
    fn parse_update_fields_accepts_500_multibyte_character_title() {
        let title = "é".repeat(500);
        let (updates, coercions) = parse_update_fields("beads_rust-1234", &json!({"title": title}))
            .expect("500-character title should be valid even when UTF-8 encoded");

        assert!(coercions.is_empty());
        let expected = "é".repeat(500);
        assert_eq!(updates.title.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn parse_update_fields_rejects_blank_title() {
        let err = parse_update_fields("beads_rust-1234", &json!({"title": "   \t"}))
            .expect_err("blank title must be rejected");

        assert!(err.to_string().contains("Title must be 1-500 characters"));
    }

    #[test]
    fn optional_string_array_arg_rejects_non_array_values() {
        let err = optional_string_array_arg(&json!({"labels": "bug"}), "labels")
            .expect_err("string labels value must be rejected");

        assert!(
            err.to_string()
                .contains("'labels' must be an array of strings")
        );
    }

    #[test]
    fn optional_string_array_arg_rejects_non_string_entries() {
        let err = optional_string_array_arg(&json!({"labels": ["bug", 42]}), "labels")
            .expect_err("non-string label entries must be rejected");

        assert!(err.to_string().contains("'labels[1]' must be a string"));
    }

    #[test]
    fn optional_label_array_arg_rejects_invalid_label_values() {
        let err = optional_label_array_arg(&json!({"labels": ["bug", "bad label"]}), "labels")
            .expect_err("MCP labels must use the same validation rules as CLI labels");

        assert!(err.to_string().contains("'labels[1]' invalid label"));
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn optional_label_array_arg_accepts_namespaced_labels() {
        let labels =
            optional_label_array_arg(&json!({"labels": ["bug", "team:backend"]}), "labels")
                .expect("valid labels");

        assert_eq!(labels, vec!["bug", "team:backend"]);
    }

    #[test]
    fn create_issue_accepts_long_description_through_mcp() {
        // Long-text issue fields are explicitly unbounded — spec write-ups,
        // RFC text, etc. routinely exceed the prior 100KB cap. Verify the
        // MCP path accepts and persists them.
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let tool = CreateIssueTool::new(Arc::clone(&state));
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let description = "x".repeat(600_000);

        tool.call(
            &ctx,
            json!({
                "title": "MCP long-description acceptance",
                "description": description
            }),
        )
        .expect("MCP create must accept long descriptions");

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let ids = storage.get_all_ids().expect("ids");
        assert_eq!(ids.len(), 1, "long-description issue must be persisted");
        let issue = storage
            .get_issue(&ids[0])
            .expect("lookup")
            .expect("issue present");
        assert_eq!(
            issue.description.as_deref().map(str::len),
            Some(600_000),
            "long description preserved verbatim"
        );
    }

    #[test]
    fn create_issue_accepts_500_multibyte_character_title() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let tool = CreateIssueTool::new(Arc::clone(&state));
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let title = "é".repeat(500);

        tool.call(&ctx, json!({ "title": title }))
            .expect("500-character title should pass MCP validation");

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let ids = storage.get_all_ids().expect("ids");
        assert_eq!(ids.len(), 1);
        let issue = storage
            .get_issue(&ids[0])
            .expect("get issue")
            .expect("issue exists");
        assert_eq!(issue.title, "é".repeat(500));
    }

    #[test]
    fn create_issue_persists_canonical_content_hash() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let tool = CreateIssueTool::new(Arc::clone(&state));
        let ctx = McpContext::new(Cx::for_testing(), 1);

        tool.call(
            &ctx,
            json!({
                "title": "MCP hash parity",
                "description": "Created through the MCP surface"
            }),
        )
        .expect("create issue");

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let ids = storage.get_all_ids().expect("ids");
        assert_eq!(ids.len(), 1);
        let issue = storage
            .get_issue(&ids[0])
            .expect("get issue")
            .expect("issue exists");
        let expected_hash = issue.compute_content_hash();

        assert_eq!(
            issue.content_hash.as_deref(),
            Some(expected_hash.as_str()),
            "MCP-created issues should persist the same content hash as CLI-created issues"
        );
    }

    #[test]
    fn manage_dependencies_allows_non_blocking_reverse_links() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let create_tool = CreateIssueTool::new(Arc::clone(&state));

        create_tool
            .call(&ctx, json!({ "title": "First dependency endpoint" }))
            .expect("create first issue");
        create_tool
            .call(&ctx, json!({ "title": "Second dependency endpoint" }))
            .expect("create second issue");

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let ids = storage.get_all_ids().expect("ids");
        assert_eq!(ids.len(), 2);
        drop(storage);

        let first = ids[0].clone();
        let second = ids[1].clone();
        let deps_tool = ManageDependenciesTool::new(Arc::clone(&state));

        deps_tool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "id": second.clone(),
                    "depends_on": first.clone(),
                    "dep_type": "blocks"
                }),
            )
            .expect("add blocking dependency");

        deps_tool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "id": first.clone(),
                    "depends_on": second.clone(),
                    "dep_type": "related"
                }),
            )
            .expect("related reverse link should not be rejected as a blocking cycle");

        let storage = SqliteStorage::open(&state.db_path).expect("reopen storage");
        let deps = storage
            .get_dependencies_full(&first)
            .expect("load dependencies");
        assert!(
            deps.iter().any(|dep| {
                dep.depends_on_id == second && dep.dep_type == DependencyType::Related
            })
        );
    }

    #[test]
    fn manage_dependencies_allows_external_targets() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-external-src", "external dependency source");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ManageDependenciesTool::new(Arc::clone(&state));

        let add = content_json(
            &tool
                .call(
                    &ctx,
                    json!({
                        "action": "add",
                        "id": "br-mcp-external-src",
                        "depends_on": "external:api:auth",
                        "dep_type": "blocks"
                    }),
                )
                .expect("add external dependency"),
        );
        assert_eq!(add["added"].as_bool(), Some(true));
        assert_eq!(add["to"].as_str(), Some("external:api:auth"));

        let list = content_json(
            &tool
                .call(
                    &ctx,
                    json!({
                        "action": "list",
                        "id": "br-mcp-external-src",
                    }),
                )
                .expect("list dependencies"),
        );
        let depends_on = list["depends_on"]
            .as_array()
            .expect("depends_on should be array");
        assert!(
            depends_on.iter().any(|dep| {
                dep["id"]
                    .as_str()
                    .is_some_and(|id| id.eq("external:api:auth"))
                    && dep["dep_type"].as_str() == Some("blocks")
            }),
            "MCP list should include external dependency: {list}"
        );

        let remove = content_json(
            &tool
                .call(
                    &ctx,
                    json!({
                        "action": "remove",
                        "id": "br-mcp-external-src",
                        "depends_on": "external:api:auth",
                    }),
                )
                .expect("remove external dependency"),
        );
        assert_eq!(remove["removed"].as_bool(), Some(true));
    }

    #[test]
    fn manage_dependencies_legacy_single_result_shapes_are_unchanged() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-dep-single-a", "single dependency source");
        insert_test_issue(&state, "br-mcp-dep-single-b", "single dependency target");
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ManageDependenciesTool::new(Arc::clone(&state));

        let add = content_json(
            &tool
                .call(
                    &ctx,
                    json!({
                        "action": "add",
                        "id": "br-mcp-dep-single-a",
                        "depends_on": "br-mcp-dep-single-b",
                        "dep_type": "parent_child"
                    }),
                )
                .expect("single add dependency"),
        );
        assert_eq!(add["added"].as_bool(), Some(true));
        assert_eq!(add["from"].as_str(), Some("br-mcp-dep-single-a"));
        assert_eq!(add["to"].as_str(), Some("br-mcp-dep-single-b"));
        assert_eq!(add["dep_type"].as_str(), Some("parent-child"));
        assert!(add.get("items").is_none());
        assert!(add.get("count").is_none());

        let list = content_json(
            &tool
                .call(
                    &ctx,
                    json!({
                        "action": "list",
                        "id": "br-mcp-dep-single-a",
                    }),
                )
                .expect("single list dependencies"),
        );
        assert_eq!(list["id"].as_str(), Some("br-mcp-dep-single-a"));
        assert!(list.get("items").is_none());
        assert!(list.get("count").is_none());
    }

    #[test]
    fn manage_dependencies_batch_returns_ordered_items_partial_errors_and_flushes() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        insert_test_issue(&state, "br-mcp-dep-batch-a", "batch dependency source a");
        insert_test_issue(&state, "br-mcp-dep-batch-b", "batch dependency target b");
        insert_test_issue(
            &state,
            "br-mcp-dep-batch-remove",
            "batch dependency pre-existing remove target",
        );
        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            storage
                .add_dependency(
                    "br-mcp-dep-batch-a",
                    "br-mcp-dep-batch-remove",
                    "blocks",
                    "mcp-test",
                )
                .expect("seed removable dependency");
        }

        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ManageDependenciesTool::new(Arc::clone(&state));
        let content = tool
            .call(
                &ctx,
                json!({
                    "operations": [
                        {
                            "action": "add",
                            "id": "br-mcp-dep-batch-a",
                            "depends_on": "br-mcp-dep-batch-b"
                        },
                        {
                            "action": "add",
                            "id": "br-mcp-dep-batch-b",
                            "depends_on": "br-mcp-dep-batch-a"
                        },
                        {
                            "action": "add",
                            "id": "br-mcp-dep-batch-a",
                            "depends_on": "YOUR_ID"
                        },
                        {
                            "action": "remove",
                            "id": "br-mcp-dep-batch-a",
                            "depends_on": "br-mcp-dep-batch-remove"
                        },
                        {
                            "action": "list",
                            "id": "br-mcp-dep-batch-a"
                        }
                    ]
                }),
            )
            .expect("batch manage_dependencies");
        let batch = content_json(&content);

        assert_batch_counts(&batch, 5, 3, 2);
        assert_eq!(batch["items"][0]["index"].as_u64(), Some(0));
        assert_eq!(batch["items"][0]["ok"].as_bool(), Some(true));
        assert_eq!(batch["items"][0]["result"]["added"].as_bool(), Some(true));
        assert_eq!(
            batch["items"][1]["error"]["data"]["error_type"].as_str(),
            Some("CYCLE_DETECTED")
        );
        assert_eq!(
            batch["items"][2]["error"]["data"]["error_type"].as_str(),
            Some("PLACEHOLDER_DETECTED")
        );
        assert_eq!(batch["items"][3]["result"]["removed"].as_bool(), Some(true));
        let listed = batch["items"][4]["result"]["depends_on"]
            .as_array()
            .expect("list result dependencies");
        assert!(listed.iter().any(|dep| {
            dep["id"].as_str() == Some("br-mcp-dep-batch-b")
                && dep["dep_type"].as_str() == Some("blocks")
        }));
        assert!(
            listed
                .iter()
                .all(|dep| dep["id"].as_str() != Some("br-mcp-dep-batch-remove"))
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let deps = storage
            .get_dependencies_full("br-mcp-dep-batch-a")
            .expect("load dependencies");
        assert!(
            deps.iter()
                .any(|dep| dep.depends_on_id == "br-mcp-dep-batch-b")
        );
        assert!(
            deps.iter()
                .all(|dep| dep.depends_on_id != "br-mcp-dep-batch-remove")
        );
        let jsonl = fs::read_to_string(&state.jsonl_path).expect("read auto-flushed jsonl");
        assert!(jsonl.contains("br-mcp-dep-batch-a"));
        assert!(jsonl.contains("br-mcp-dep-batch-b"));
    }

    #[test]
    fn manage_dependencies_batch_rejects_ambiguous_single_and_batch_args() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let tool = ManageDependenciesTool::new(state);

        let err = tool
            .call(
                &ctx,
                json!({
                    "action": "list",
                    "id": "br-one",
                    "operations": [{"action": "list", "id": "br-two"}]
                }),
            )
            .expect_err("single and batch args together should fail");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(err.message.contains("either action/id"));
    }

    #[test]
    fn create_issue_rejects_tombstone_parent_without_persisting_child() {
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let tool = CreateIssueTool::new(Arc::clone(&state));
        let ctx = McpContext::new(Cx::for_testing(), 1);

        {
            let mut storage = SqliteStorage::open(&state.db_path).expect("open storage");
            let now = Utc::now();
            let parent = Issue {
                id: "br-parent".to_string(),
                title: "Deleted parent".to_string(),
                status: Status::Open,
                priority: Priority::MEDIUM,
                issue_type: IssueType::Epic,
                created_at: now,
                updated_at: now,
                ..Issue::default()
            };
            storage
                .create_issue(&parent, "mcp-test")
                .expect("create parent");
            storage
                .delete_issue("br-parent", "mcp-test", "delete parent", None)
                .expect("delete parent");
        }

        let err = tool
            .call(
                &ctx,
                json!({
                    "title": "Child should not persist",
                    "parent": "br-parent"
                }),
            )
            .expect_err("MCP create must reject tombstone parents before mutating");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(
            err.message.contains("tombstoned"),
            "unexpected MCP error: {err:?}"
        );

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        assert!(
            storage
                .get_issue("br-parent.1")
                .expect("lookup child")
                .is_none(),
            "child issue must not be persisted after tombstone parent rejection"
        );
    }

    #[test]
    fn update_issue_rejects_invalid_comment_before_field_mutation() {
        // The transactional pre-flight invariant: when an UpdateIssue MCP call
        // bundles a comment and a field mutation, an invalid comment must
        // reject the entire call (no partial application). Comment size is
        // no longer a validation axis, so this test uses an embedded NUL byte
        // — still rejected by `reject_nul` for SQLite compatibility.
        let temp = TempDir::new().expect("tempdir");
        let state = mcp_test_state(&temp);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let create_tool = CreateIssueTool::new(Arc::clone(&state));

        create_tool
            .call(&ctx, json!({"title": "Original MCP title"}))
            .expect("create issue");

        let storage = SqliteStorage::open(&state.db_path).expect("open storage");
        let ids = storage.get_all_ids().expect("ids");
        assert_eq!(ids.len(), 1);
        drop(storage);

        let update_tool = UpdateIssueTool::new(Arc::clone(&state));
        let err = update_tool
            .call(
                &ctx,
                json!({
                    "id": ids[0],
                    "title": "Mutated despite comment error",
                    "comment": "valid prefix\0NUL byte poisons this comment"
                }),
            )
            .expect_err("NUL-byte comment must reject the whole MCP update");

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        assert!(
            err.message.contains("content") && err.message.contains("NUL"),
            "unexpected MCP error: {err:?}"
        );

        let storage = SqliteStorage::open(&state.db_path).expect("reopen storage");
        let issue = storage
            .get_issue(&ids[0])
            .expect("get issue")
            .expect("issue exists");
        assert_eq!(issue.title, "Original MCP title");
        assert!(
            storage.get_comments(&ids[0]).expect("comments").is_empty(),
            "invalid MCP comment must not be inserted"
        );
    }

    #[test]
    fn build_list_filters_rejects_wrong_limit_type() {
        let mut coercions = Vec::new();
        let err = build_list_filters(&json!({"limit": "10"}), &mut coercions)
            .expect_err("string limit must be rejected");

        assert!(
            err.to_string()
                .contains("'limit' must be a non-negative integer")
        );
    }

    #[test]
    fn issue_not_found_err_surfaces_id_scan_failure() {
        let storage = SqliteStorage::open_memory().expect("storage");
        storage
            .execute_raw("DROP TABLE issues")
            .expect("drop issues table");

        let err = issue_not_found_err(&storage, "bd-missing")
            .expect_err("ID scan failure must be returned to MCP clients");

        assert!(
            err.to_string().contains("issues") || err.to_string().contains("no such table"),
            "unexpected MCP error: {err}"
        );
    }

    #[test]
    fn hash_id_generation_reports_lookup_failure_even_if_later_candidate_is_free() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 4, 22, 20, 55, 0)
            .single()
            .expect("valid timestamp");
        let calls = Cell::new(0);

        let err = generate_issue_id_with_checked_lookup(
            "MCP lookup failure",
            "mcp-test",
            now,
            "br",
            0,
            |_| {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    Err(BeadsError::Config("id lookup unavailable".to_string()))
                } else {
                    Ok(false)
                }
            },
        )
        .expect_err("any ID lookup error must be returned");

        assert_eq!(err.code, fastmcp_rust::McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("ID_LOOKUP_FAILED")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(serde_json::Value::as_str),
            Some("hash ID generation")
        );
    }

    #[test]
    fn hash_id_generation_uses_mcp_database_issue_count() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 7, 11, 12, 0, 0)
            .single()
            .expect("valid timestamp");
        let issue_count = 100_000;
        let expected_length = crate::util::id::IdGenerator::new(
            crate::util::id::IdConfig::with_prefix("br").expect("valid prefix"),
        )
        .optimal_length(issue_count);

        let id = generate_issue_id_with_checked_lookup(
            "MCP adaptive length",
            "mcp-test",
            now,
            "br",
            issue_count,
            |_| Ok(false),
        )
        .expect("empty registry produces an ID");
        let parsed = crate::util::id::parse_id(&id).expect("generated ID parses");

        assert!(expected_length > 3, "test count must widen the hash");
        assert_eq!(parsed.hash.len(), expected_length);
    }

    #[test]
    fn child_id_generation_reports_lookup_failure() {
        let err = next_available_child_id("br-parent", 1, |_| {
            Err(BeadsError::Config("child lookup unavailable".to_string()))
        })
        .expect_err("child ID lookup error must be returned");

        assert_eq!(err.code, fastmcp_rust::McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("ID_LOOKUP_FAILED")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(serde_json::Value::as_str),
            Some("child ID generation")
        );
    }

    #[test]
    fn child_id_generation_bounds_collision_retries() {
        let err = next_available_child_id("br-parent", 7, |_| Ok(true))
            .expect_err("fully occupied child ID window must fail explicitly");

        assert_eq!(err.code, fastmcp_rust::McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("NO_AVAILABLE_CHILD_ID")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("first_candidate_number"))
                .and_then(serde_json::Value::as_u64),
            Some(7)
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("last_candidate_number"))
                .and_then(serde_json::Value::as_u64),
            Some(107)
        );
    }

    #[test]
    fn storage_read_warning_carries_structured_source_error() {
        let warning = storage_read_warning(
            "get_blockers",
            &BeadsError::Config("dependency lookup failed".to_string()),
        );

        assert_eq!(warning["warning_type"], "STORAGE_READ_FAILED");
        assert_eq!(warning["operation"], "get_blockers");
        assert_eq!(
            warning["message"],
            "Configuration error: dependency lookup failed"
        );
    }
}
