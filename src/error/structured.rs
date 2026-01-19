//! Structured error output for AI coding agents.
//!
//! Provides machine-parseable error information with:
//! - Error codes for categorization
//! - Hints for self-correction
//! - Retryability flags
//! - Context for debugging
//!
//! # Design Patterns (from `mcp_agent_mail`)
//!
//! This module adapts the structured error pattern from `mcp_agent_mail`.
//! Key concepts:
//!
//! - Intent detection: Recognize common agent mistakes
//! - O(1) validation: Precomputed valid value sets
//! - Levenshtein suggestions: Find similar IDs
//! - Graceful defaults: Auto-fix what you can

#![allow(clippy::option_if_let_else, clippy::manual_map, clippy::manual_find)]

use crate::error::BeadsError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

/// Machine-readable error codes.
///
/// These codes are stable and can be used for programmatic error handling.
/// Format: `SCREAMING_SNAKE_CASE` for easy parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    // === Database Errors (exit code 2) ===
    /// Database file not found
    DatabaseNotFound,
    /// Database is locked by another process
    DatabaseLocked,
    /// Database schema version mismatch
    SchemaMismatch,
    /// Database operation failed
    DatabaseError,
    /// Beads workspace not initialized
    NotInitialized,
    /// Already initialized
    AlreadyInitialized,

    // === Issue Errors (exit code 3) ===
    /// Issue with specified ID not found
    IssueNotFound,
    /// Partial ID matches multiple issues
    AmbiguousId,
    /// Issue ID collision on create
    IdCollision,
    /// Invalid issue ID format
    InvalidId,

    // === Validation Errors (exit code 4) ===
    /// Field validation failed
    ValidationFailed,
    /// Invalid status value
    InvalidStatus,
    /// Invalid issue type value
    InvalidType,
    /// Priority out of range (0-4)
    InvalidPriority,
    /// Required field missing
    RequiredField,

    // === Dependency Errors (exit code 5) ===
    /// Dependency cycle detected
    CycleDetected,
    /// Dependency target not found
    DependencyNotFound,
    /// Cannot delete: has dependents
    HasDependents,
    /// Issue cannot depend on itself
    SelfDependency,
    /// Duplicate dependency
    DuplicateDependency,

    // === Sync/JSONL Errors (exit code 6) ===
    /// JSONL parse error
    JsonlParseError,
    /// Prefix mismatch during import
    PrefixMismatch,
    /// Import collision detected
    ImportCollision,
    /// Conflict markers in JSONL
    ConflictMarkers,
    /// Path traversal attempt blocked
    PathTraversal,

    // === Config Errors (exit code 7) ===
    /// Configuration error
    ConfigError,
    /// Config file not found
    ConfigNotFound,
    /// Config parse error
    ConfigParseError,

    // === I/O Errors (exit code 8) ===
    /// File I/O error
    IoError,
    /// JSON serialization error
    JsonError,
    /// YAML parsing error
    YamlError,

    // === Internal Errors (exit code 1) ===
    /// Unexpected internal error
    InternalError,
}

impl ErrorCode {
    /// Get the string representation for JSON output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            // Database
            Self::DatabaseNotFound => "DATABASE_NOT_FOUND",
            Self::DatabaseLocked => "DATABASE_LOCKED",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::DatabaseError => "DATABASE_ERROR",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::AlreadyInitialized => "ALREADY_INITIALIZED",
            // Issue
            Self::IssueNotFound => "ISSUE_NOT_FOUND",
            Self::AmbiguousId => "AMBIGUOUS_ID",
            Self::IdCollision => "ID_COLLISION",
            Self::InvalidId => "INVALID_ID",
            // Validation
            Self::ValidationFailed => "VALIDATION_FAILED",
            Self::InvalidStatus => "INVALID_STATUS",
            Self::InvalidType => "INVALID_TYPE",
            Self::InvalidPriority => "INVALID_PRIORITY",
            Self::RequiredField => "REQUIRED_FIELD",
            // Dependency
            Self::CycleDetected => "CYCLE_DETECTED",
            Self::DependencyNotFound => "DEPENDENCY_NOT_FOUND",
            Self::HasDependents => "HAS_DEPENDENTS",
            Self::SelfDependency => "SELF_DEPENDENCY",
            Self::DuplicateDependency => "DUPLICATE_DEPENDENCY",
            // Sync
            Self::JsonlParseError => "JSONL_PARSE_ERROR",
            Self::PrefixMismatch => "PREFIX_MISMATCH",
            Self::ImportCollision => "IMPORT_COLLISION",
            Self::ConflictMarkers => "CONFLICT_MARKERS",
            Self::PathTraversal => "PATH_TRAVERSAL",
            // Config
            Self::ConfigError => "CONFIG_ERROR",
            Self::ConfigNotFound => "CONFIG_NOT_FOUND",
            Self::ConfigParseError => "CONFIG_PARSE_ERROR",
            // I/O
            Self::IoError => "IO_ERROR",
            Self::JsonError => "JSON_ERROR",
            Self::YamlError => "YAML_ERROR",
            // Internal
            Self::InternalError => "INTERNAL_ERROR",
        }
    }

    /// Whether this error is potentially retryable.
    ///
    /// Retryable means the agent might succeed if it:
    /// - Waits and retries (e.g., database locked)
    /// - Fixes the input and retries (e.g., validation error)
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseLocked
                | Self::ValidationFailed
                | Self::InvalidStatus
                | Self::InvalidType
                | Self::InvalidPriority
                | Self::RequiredField
                | Self::AmbiguousId
        )
    }

    /// Get the exit code for this error category.
    ///
    /// Exit codes are grouped by error category:
    /// - 1: Internal/unknown errors
    /// - 2: Database errors
    /// - 3: Issue errors
    /// - 4: Validation errors
    /// - 5: Dependency errors
    /// - 6: Sync/JSONL errors
    /// - 7: Config errors
    /// - 8: I/O errors
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            // Database (2)
            Self::DatabaseNotFound
            | Self::DatabaseLocked
            | Self::SchemaMismatch
            | Self::DatabaseError
            | Self::NotInitialized
            | Self::AlreadyInitialized => 2,
            // Issue (3)
            Self::IssueNotFound | Self::AmbiguousId | Self::IdCollision | Self::InvalidId => 3,
            // Validation (4)
            Self::ValidationFailed
            | Self::InvalidStatus
            | Self::InvalidType
            | Self::InvalidPriority
            | Self::RequiredField => 4,
            // Dependency (5)
            Self::CycleDetected
            | Self::DependencyNotFound
            | Self::HasDependents
            | Self::SelfDependency
            | Self::DuplicateDependency => 5,
            // Sync (6)
            Self::JsonlParseError
            | Self::PrefixMismatch
            | Self::ImportCollision
            | Self::ConflictMarkers
            | Self::PathTraversal => 6,
            // Config (7)
            Self::ConfigError | Self::ConfigNotFound | Self::ConfigParseError => 7,
            // I/O (8)
            Self::IoError | Self::JsonError | Self::YamlError => 8,
            // Internal (1)
            Self::InternalError => 1,
        }
    }
}

/// Structured error for machine-parseable output.
///
/// Provides AI coding agents with:
/// - Machine-readable error code
/// - Human-readable message
/// - Context-aware hint for self-correction
/// - Retryability flag
/// - Structured context data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredError {
    /// Machine-readable error code
    pub code: ErrorCode,
    /// Human-readable error message
    pub message: String,
    /// Optional hint for fixing the error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether the operation can be retried
    pub retryable: bool,
    /// Additional context data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

impl StructuredError {
    /// Create a new structured error from a `BeadsError`.
    #[must_use]
    pub fn from_error(err: &BeadsError) -> Self {
        let (code, context) = Self::extract_code_and_context(err);
        let hint = Self::generate_hint(err, context.as_ref());

        Self {
            code,
            message: err.to_string(),
            hint,
            retryable: code.is_retryable(),
            context,
        }
    }

    /// Create a structured error with similar ID suggestions.
    #[must_use]
    pub fn issue_not_found(searched_id: &str, existing_ids: &[String]) -> Self {
        let similar = find_similar_ids(searched_id, existing_ids, 3);

        let hint = if similar.is_empty() {
            Some("Run 'br list' to see available issues.".to_string())
        } else if similar.len() == 1 {
            Some(format!("Did you mean '{}'?", similar[0]))
        } else {
            Some(format!("Did you mean one of: {}?", similar.join(", ")))
        };

        let context = json!({
            "searched_id": searched_id,
            "similar_ids": similar,
        });

        Self {
            code: ErrorCode::IssueNotFound,
            message: format!("Issue not found: {searched_id}"),
            hint,
            retryable: false,
            context: Some(context),
        }
    }

    /// Create a structured error for ambiguous ID.
    #[must_use]
    pub fn ambiguous_id(partial: &str, matches: &[String]) -> Self {
        let hint = Some(format!(
            "Provide more characters to disambiguate. Matches: {}",
            matches.join(", ")
        ));

        let context = json!({
            "partial_id": partial,
            "matches": matches,
            "match_count": matches.len(),
        });

        Self {
            code: ErrorCode::AmbiguousId,
            message: format!(
                "Ambiguous ID '{}': matches {} issues",
                partial,
                matches.len()
            ),
            hint,
            retryable: true,
            context: Some(context),
        }
    }

    /// Create a structured error for cycle detection.
    #[must_use]
    pub fn cycle_detected(cycle_path: &str) -> Self {
        let parts: Vec<&str> = cycle_path.split(" -> ").collect();

        let context = json!({
            "cycle_path": cycle_path,
            "cycle_nodes": parts,
        });

        Self {
            code: ErrorCode::CycleDetected,
            message: format!("Cycle detected in dependencies: {cycle_path}"),
            hint: Some("Remove one dependency to break the cycle.".to_string()),
            retryable: false,
            context: Some(context),
        }
    }

    /// Create a structured error for not initialized.
    #[must_use]
    pub fn not_initialized() -> Self {
        Self {
            code: ErrorCode::NotInitialized,
            message: "Beads not initialized: run 'br init' first".to_string(),
            hint: Some("Run: br init".to_string()),
            retryable: false,
            context: None,
        }
    }

    /// Create a structured error for invalid priority.
    #[must_use]
    pub fn invalid_priority(provided: &str) -> Self {
        let hint = if let Some(detected) = detect_priority_intent(provided) {
            Some(format!(
                "Did you mean --priority {detected}? Priority must be 0-4 (or P0-P4): 0=critical, 1=high, 2=medium, 3=low, 4=backlog"
            ))
        } else {
            Some(
                "Priority must be 0-4 (or P0-P4): 0=critical, 1=high, 2=medium, 3=low, 4=backlog"
                    .to_string(),
            )
        };

        let context = json!({
            "provided": provided,
            "valid_values": ["0", "1", "2", "3", "4", "P0", "P1", "P2", "P3", "P4"],
            "priority_mapping": {
                "0": "critical",
                "1": "high",
                "2": "medium",
                "3": "low",
                "4": "backlog"
            }
        });

        Self {
            code: ErrorCode::InvalidPriority,
            message: format!("Invalid priority: {provided}"),
            hint,
            retryable: true,
            context: Some(context),
        }
    }

    /// Create a structured error for invalid status.
    #[must_use]
    pub fn invalid_status(provided: &str) -> Self {
        let hint = if let Some(detected) = detect_status_intent(provided) {
            Some(format!("Did you mean --status {detected}?"))
        } else {
            Some("Valid statuses: open, in_progress, blocked, deferred, closed".to_string())
        };

        let context = json!({
            "provided": provided,
            "valid_values": VALID_STATUSES.iter().collect::<Vec<_>>(),
        });

        Self {
            code: ErrorCode::InvalidStatus,
            message: format!("Invalid status: {provided}"),
            hint,
            retryable: true,
            context: Some(context),
        }
    }

    /// Create a structured error for invalid issue type.
    #[must_use]
    pub fn invalid_type(provided: &str) -> Self {
        let hint = if let Some(detected) = detect_type_intent(provided) {
            Some(format!("Did you mean --type {detected}?"))
        } else {
            Some("Valid types: task, bug, feature, epic, chore, docs, question".to_string())
        };

        let context = json!({
            "provided": provided,
            "valid_values": VALID_TYPES.iter().collect::<Vec<_>>(),
        });

        Self {
            code: ErrorCode::InvalidType,
            message: format!("Invalid issue type: {provided}"),
            hint,
            retryable: true,
            context: Some(context),
        }
    }

    /// Serialize to JSON value.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
                "hint": self.hint,
                "retryable": self.retryable,
                "context": self.context,
            }
        })
    }

    /// Format for human-readable output.
    #[must_use]
    pub fn to_human(&self, color: bool) -> String {
        let mut output = String::new();

        if color {
            // Red for error
            output.push_str("\x1b[31mError:\x1b[0m ");
        } else {
            output.push_str("Error: ");
        }

        output.push_str(&self.message);

        if let Some(hint) = &self.hint {
            output.push('\n');
            if color {
                // Yellow for hint
                output.push_str("\x1b[33mHint:\x1b[0m ");
            } else {
                output.push_str("Hint: ");
            }
            output.push_str(hint);
        }

        output
    }

    /// Extract error code and context from a `BeadsError`.
    #[allow(clippy::too_many_lines)]
    fn extract_code_and_context(err: &BeadsError) -> (ErrorCode, Option<Value>) {
        match err {
            BeadsError::DatabaseNotFound { path } => (
                ErrorCode::DatabaseNotFound,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::DatabaseLocked { path } => (
                ErrorCode::DatabaseLocked,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::SchemaMismatch { expected, found } => (
                ErrorCode::SchemaMismatch,
                Some(json!({"expected": expected, "found": found})),
            ),
            BeadsError::Database(_) => (ErrorCode::DatabaseError, None),
            BeadsError::NotInitialized => (ErrorCode::NotInitialized, None),
            BeadsError::AlreadyInitialized { path } => (
                ErrorCode::AlreadyInitialized,
                Some(json!({"path": path.display().to_string()})),
            ),
            BeadsError::IssueNotFound { id } => {
                (ErrorCode::IssueNotFound, Some(json!({"searched_id": id})))
            }
            BeadsError::AmbiguousId { partial, matches } => (
                ErrorCode::AmbiguousId,
                Some(json!({"partial_id": partial, "matches": matches})),
            ),
            BeadsError::IdCollision { id } => (ErrorCode::IdCollision, Some(json!({"id": id}))),
            BeadsError::InvalidId { id } => (ErrorCode::InvalidId, Some(json!({"id": id}))),
            BeadsError::Validation { field, reason } => (
                ErrorCode::ValidationFailed,
                Some(json!({"field": field, "reason": reason})),
            ),
            BeadsError::ValidationErrors { errors } => (
                ErrorCode::ValidationFailed,
                Some(json!({
                    "errors": errors.iter()
                        .map(|e| json!({"field": e.field, "message": e.message}))
                        .collect::<Vec<_>>()
                })),
            ),
            BeadsError::InvalidStatus { status } => {
                let hint = detect_status_intent(status)
                    .map(|detected| format!("Did you mean --status {detected}?"));

                (
                    ErrorCode::InvalidStatus,
                    Some(serde_json::json!({
                        "status": status,
                        "hint": hint
                    })),
                )
            }
            BeadsError::InvalidType { issue_type } => {
                let hint = detect_type_intent(issue_type)
                    .map(|detected| format!("Did you mean --type {detected}?"));

                (
                    ErrorCode::InvalidType,
                    Some(serde_json::json!({
                        "issue_type": issue_type,
                        "hint": hint
                    })),
                )
            }
            BeadsError::InvalidPriority { priority } => {
                let hint = detect_priority_intent(&priority.to_string()).map_or_else(
                    || Some("Priority must be 0-4 (0=critical, 4=backlog).".to_string()),
                    |detected| Some(format!("Did you mean --priority {detected}?")),
                );

                (
                    ErrorCode::InvalidPriority,
                    Some(serde_json::json!({
                        "priority": priority,
                        "hint": hint
                    })),
                )
            }
            BeadsError::JsonlParse { line, reason } => (
                ErrorCode::JsonlParseError,
                Some(json!({"line": line, "reason": reason})),
            ),
            BeadsError::PrefixMismatch { expected, found } => (
                ErrorCode::PrefixMismatch,
                Some(json!({"expected": expected, "found": found})),
            ),
            BeadsError::ImportCollision { count } => (
                ErrorCode::ImportCollision,
                Some(json!({"collision_count": count})),
            ),
            BeadsError::DependencyCycle { path } => {
                (ErrorCode::CycleDetected, Some(json!({"cycle_path": path})))
            }
            BeadsError::HasDependents { id, count } => (
                ErrorCode::HasDependents,
                Some(json!({"id": id, "dependent_count": count})),
            ),
            BeadsError::SelfDependency { id } => {
                (ErrorCode::SelfDependency, Some(json!({"id": id})))
            }
            BeadsError::DependencyNotFound { id } => {
                (ErrorCode::DependencyNotFound, Some(json!({"id": id})))
            }
            BeadsError::DuplicateDependency { from, to } => (
                ErrorCode::DuplicateDependency,
                Some(json!({"from": from, "to": to})),
            ),
            BeadsError::Config(_) => (ErrorCode::ConfigError, None),
            BeadsError::Io(_) => (ErrorCode::IoError, None),
            BeadsError::Json(_) => (ErrorCode::JsonError, None),
            BeadsError::Yaml(_) => (ErrorCode::YamlError, None),
            BeadsError::WithContext { context, .. } => {
                (ErrorCode::InternalError, Some(json!({"context": context})))
            }
            BeadsError::Other(_) => (ErrorCode::InternalError, None),
        }
    }

    /// Generate context-aware hint from error.
    fn generate_hint(err: &BeadsError, context: Option<&Value>) -> Option<String> {
        // First check if BeadsError has a built-in suggestion
        if let Some(suggestion) = err.suggestion() {
            return Some(suggestion.to_string());
        }

        // Generate additional hints based on context
        match err {
            BeadsError::IssueNotFound { .. } => {
                Some("Run 'br list' to see available issues.".to_string())
            }
            BeadsError::InvalidPriority { priority } => {
                if let Some(detected) = detect_priority_intent(&priority.to_string()) {
                    Some(format!("Did you mean --priority {detected}?"))
                } else {
                    Some("Priority must be 0-4 (0=critical, 4=backlog).".to_string())
                }
            }
            BeadsError::InvalidStatus { status } => {
                if let Some(detected) = detect_status_intent(status) {
                    Some(format!("Did you mean --status {detected}?"))
                } else {
                    None
                }
            }
            BeadsError::InvalidType { issue_type } => {
                if let Some(detected) = detect_type_intent(issue_type) {
                    Some(format!("Did you mean --type {detected}?"))
                } else {
                    None
                }
            }
            BeadsError::HasDependents { id, .. } => {
                if let Some(ctx) = context {
                    if let Some(count) = ctx.get("dependent_count") {
                        return Some(format!(
                            "Use --force to delete anyway, or close {count} dependents first."
                        ));
                    }
                }
                Some(format!("Use --force to delete '{id}' anyway."))
            }
            BeadsError::JsonlParse { line, .. } => Some(format!(
                "Check line {line} of the JSONL file for syntax errors."
            )),
            _ => None,
        }
    }
}

// === Precomputed Valid Values (O(1) lookup) ===

/// Valid status values.
static VALID_STATUSES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "open",
        "in_progress",
        "blocked",
        "deferred",
        "closed",
        "tombstone",
    ]
    .into_iter()
    .collect()
});

/// Valid issue type values.
static VALID_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "task", "bug", "feature", "epic", "chore", "docs", "question",
    ]
    .into_iter()
    .collect()
});

/// Status synonyms for intent detection.
static STATUS_SYNONYMS: LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        [
            ("done", "closed"),
            ("complete", "closed"),
            ("completed", "closed"),
            ("finished", "closed"),
            ("resolved", "closed"),
            ("wontfix", "closed"),
            ("wip", "in_progress"),
            ("working", "in_progress"),
            ("active", "in_progress"),
            ("started", "in_progress"),
            ("new", "open"),
            ("todo", "open"),
            ("pending", "open"),
            ("waiting", "blocked"),
            ("hold", "deferred"),
            ("later", "deferred"),
            ("postponed", "deferred"),
        ]
        .into_iter()
        .collect()
    });

/// Type synonyms for intent detection.
static TYPE_SYNONYMS: LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        [
            ("story", "feature"),
            ("enhancement", "feature"),
            ("improvement", "feature"),
            ("issue", "bug"),
            ("defect", "bug"),
            ("problem", "bug"),
            ("ticket", "task"),
            ("item", "task"),
            ("work", "task"),
            ("documentation", "docs"),
            ("doc", "docs"),
            ("readme", "docs"),
            ("cleanup", "chore"),
            ("refactor", "chore"),
            ("maintenance", "chore"),
            ("parent", "epic"),
            ("initiative", "epic"),
            ("ask", "question"),
            ("help", "question"),
        ]
        .into_iter()
        .collect()
    });

/// Priority synonyms for intent detection.
static PRIORITY_SYNONYMS: LazyLock<std::collections::HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        [
            ("critical", "0"),
            ("crit", "0"),
            ("urgent", "0"),
            ("highest", "0"),
            ("high", "1"),
            ("important", "1"),
            ("medium", "2"),
            ("normal", "2"),
            ("default", "2"),
            ("low", "3"),
            ("minor", "3"),
            ("backlog", "4"),
            ("lowest", "4"),
            ("trivial", "4"),
        ]
        .into_iter()
        .collect()
    });

// === Intent Detection ===

/// Detect what status the user likely meant.
fn detect_status_intent(input: &str) -> Option<&'static str> {
    let lower = input.to_lowercase();

    // Direct match (case-insensitive)
    if VALID_STATUSES.contains(lower.as_str()) {
        return VALID_STATUSES.get(lower.as_str()).copied();
    }

    // Synonym lookup
    if let Some(&canonical) = STATUS_SYNONYMS.get(lower.as_str()) {
        return Some(canonical);
    }

    // Prefix match
    for &status in VALID_STATUSES.iter() {
        if status.starts_with(&lower) {
            return Some(status);
        }
    }

    None
}

/// Detect what type the user likely meant.
fn detect_type_intent(input: &str) -> Option<&'static str> {
    let lower = input.to_lowercase();

    // Direct match
    if VALID_TYPES.contains(lower.as_str()) {
        return VALID_TYPES.get(lower.as_str()).copied();
    }

    // Synonym lookup
    if let Some(&canonical) = TYPE_SYNONYMS.get(lower.as_str()) {
        return Some(canonical);
    }

    // Prefix match
    for &t in VALID_TYPES.iter() {
        if t.starts_with(&lower) {
            return Some(t);
        }
    }

    None
}

/// Detect what priority the user likely meant.
fn detect_priority_intent(input: &str) -> Option<&'static str> {
    let lower = input.to_lowercase();

    // Already valid
    if ["0", "1", "2", "3", "4"].contains(&lower.as_str()) {
        return Some(match lower.as_str() {
            "0" => "0",
            "1" => "1",
            "2" => "2",
            "3" => "3",
            "4" => "4",
            _ => unreachable!(),
        });
    }

    // P0-P4 format
    if lower.starts_with('p') && lower.len() == 2 {
        let digit = lower.chars().nth(1)?;
        if digit.is_ascii_digit() && digit <= '4' {
            return Some(match digit {
                '0' => "0",
                '1' => "1",
                '2' => "2",
                '3' => "3",
                '4' => "4",
                _ => unreachable!(),
            });
        }
    }

    // Synonym lookup
    PRIORITY_SYNONYMS.get(lower.as_str()).copied()
}

// === Levenshtein Distance ===

/// Calculate the Levenshtein distance between two strings.
///
/// This is used to find similar IDs when an issue is not found.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.chars().count();
    let b_len = b.chars().count();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Levenshtein distance matrix
    let mut matrix = vec![vec![0; b_len + 1]; a_len + 1];

    for (i, row) in matrix.iter_mut().enumerate().take(a_len + 1) {
        row[0] = i;
    }
    for (j, item) in matrix[0].iter_mut().enumerate().take(b_len + 1) {
        *item = j;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();

    for (i, a_char) in a_chars.iter().enumerate() {
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            matrix[i + 1][j + 1] = std::cmp::min(
                std::cmp::min(matrix[i][j + 1] + 1, matrix[i + 1][j] + 1),
                matrix[i][j] + cost,
            );
        }
    }

    matrix[a_len][b_len]
}

/// Find IDs similar to the searched ID using Levenshtein distance.
///
/// Returns up to `max_suggestions` IDs with distance <= 3.
pub fn find_similar_ids(
    searched: &str,
    existing: &[String],
    max_suggestions: usize,
) -> Vec<String> {
    let mut candidates: Vec<(usize, &str)> = existing
        .iter()
        .map(|id| (levenshtein_distance(searched, id), id.as_str()))
        .filter(|(dist, _)| *dist <= 3) // Only suggest if reasonably close
        .collect();

    // Sort by distance, then alphabetically
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    candidates
        .into_iter()
        .take(max_suggestions)
        .map(|(_, id)| id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_as_str() {
        assert_eq!(ErrorCode::IssueNotFound.as_str(), "ISSUE_NOT_FOUND");
        assert_eq!(ErrorCode::CycleDetected.as_str(), "CYCLE_DETECTED");
        assert_eq!(ErrorCode::NotInitialized.as_str(), "NOT_INITIALIZED");
    }

    #[test]
    fn test_error_code_is_retryable() {
        assert!(!ErrorCode::IssueNotFound.is_retryable());
        assert!(!ErrorCode::CycleDetected.is_retryable());
        assert!(ErrorCode::DatabaseLocked.is_retryable());
        assert!(ErrorCode::ValidationFailed.is_retryable());
        assert!(ErrorCode::InvalidPriority.is_retryable());
    }

    #[test]
    fn test_error_code_exit_codes() {
        assert_eq!(ErrorCode::NotInitialized.exit_code(), 2);
        assert_eq!(ErrorCode::IssueNotFound.exit_code(), 3);
        assert_eq!(ErrorCode::ValidationFailed.exit_code(), 4);
        assert_eq!(ErrorCode::CycleDetected.exit_code(), 5);
        assert_eq!(ErrorCode::JsonlParseError.exit_code(), 6);
        assert_eq!(ErrorCode::ConfigError.exit_code(), 7);
        assert_eq!(ErrorCode::IoError.exit_code(), 8);
        assert_eq!(ErrorCode::InternalError.exit_code(), 1);
    }

    #[test]
    fn test_structured_error_to_json() {
        let err = StructuredError {
            code: ErrorCode::IssueNotFound,
            message: "Issue not found: bd-abc".to_string(),
            hint: Some("Did you mean 'bd-abd'?".to_string()),
            retryable: false,
            context: Some(json!({"searched_id": "bd-abc"})),
        };
        let json = err.to_json();
        assert_eq!(json["error"]["code"], "ISSUE_NOT_FOUND");
        assert_eq!(json["error"]["hint"], "Did you mean 'bd-abd'?");
        assert!(!json["error"]["retryable"].as_bool().unwrap());
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "abd"), 1);
        assert_eq!(levenshtein_distance("abc", "abcd"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn test_find_similar_ids() {
        let existing = vec![
            "bd-abc123".to_string(),
            "bd-xyz789".to_string(),
            "bd-abc124".to_string(),
            "bd-def456".to_string(),
        ];

        let suggestions = find_similar_ids("bd-abc12", &existing, 3);
        assert!(!suggestions.is_empty());
        // bd-abc123 and bd-abc124 should be closest (distance 1)
        assert!(suggestions.contains(&"bd-abc123".to_string()));
    }

    #[test]
    fn test_detect_status_intent() {
        assert_eq!(detect_status_intent("done"), Some("closed"));
        assert_eq!(detect_status_intent("wip"), Some("in_progress"));
        assert_eq!(detect_status_intent("OPEN"), Some("open"));
        assert_eq!(detect_status_intent("op"), Some("open")); // Prefix match
        assert_eq!(detect_status_intent("xyz"), None);
    }

    #[test]
    fn test_detect_type_intent() {
        assert_eq!(detect_type_intent("story"), Some("feature"));
        assert_eq!(detect_type_intent("defect"), Some("bug"));
        assert_eq!(detect_type_intent("TASK"), Some("task"));
        assert_eq!(detect_type_intent("xyz"), None);
    }

    #[test]
    fn test_detect_priority_intent() {
        assert_eq!(detect_priority_intent("high"), Some("1"));
        assert_eq!(detect_priority_intent("critical"), Some("0"));
        assert_eq!(detect_priority_intent("P2"), Some("2"));
        assert_eq!(detect_priority_intent("p3"), Some("3"));
        assert_eq!(detect_priority_intent("2"), Some("2"));
        assert_eq!(detect_priority_intent("xyz"), None);
    }

    #[test]
    fn test_structured_error_not_initialized() {
        let err = StructuredError::not_initialized();
        assert_eq!(err.code, ErrorCode::NotInitialized);
        assert!(err.hint.as_ref().unwrap().contains("br init"));
    }

    #[test]
    fn test_structured_error_invalid_priority() {
        let err = StructuredError::invalid_priority("high");
        assert_eq!(err.code, ErrorCode::InvalidPriority);
        assert!(err.hint.as_ref().unwrap().contains("--priority 1"));
        assert!(err.retryable);
    }

    #[test]
    fn test_structured_error_invalid_status() {
        let err = StructuredError::invalid_status("done");
        assert_eq!(err.code, ErrorCode::InvalidStatus);
        assert!(err.hint.as_ref().unwrap().contains("closed"));
    }

    #[test]
    fn test_structured_error_ambiguous_id() {
        let matches = vec!["bd-abc".to_string(), "bd-abd".to_string()];
        let err = StructuredError::ambiguous_id("bd-ab", &matches);
        assert_eq!(err.code, ErrorCode::AmbiguousId);
        assert!(err.retryable);
        assert!(err.context.as_ref().unwrap()["matches"].is_array());
    }

    #[test]
    fn test_to_human_output() {
        let err = StructuredError {
            code: ErrorCode::IssueNotFound,
            message: "Issue not found: bd-abc".to_string(),
            hint: Some("Did you mean 'bd-abd'?".to_string()),
            retryable: false,
            context: None,
        };

        let plain = err.to_human(false);
        assert!(plain.contains("Error: Issue not found: bd-abc"));
        assert!(plain.contains("Hint: Did you mean 'bd-abd'?"));

        let colored = err.to_human(true);
        assert!(colored.contains("\x1b[31m")); // Red color code
        assert!(colored.contains("\x1b[33m")); // Yellow color code
    }
}
