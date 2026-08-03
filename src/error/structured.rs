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

use crate::error::{BeadsError, REPLACE_FLAG, STATUS_ALL_HINT, SkippedTarget, VALID_STATUSES_HINT};
use crate::model::Status;
use crate::validation::text_guard::TextField;
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
    /// The stored value is not the value bd was asked to store
    WriteMismatch,
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
    /// A write would shrink a non-empty free-text field (refused by default)
    DestructiveUpdate,
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

    // === Operational Errors (exit code 3) ===
    /// All requested items were skipped; nothing to do
    NothingToDo,
    /// Part of a multi-id request applied and part did not
    PartiallyClosed,

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
            Self::WriteMismatch => "WRITE_MISMATCH",
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
            Self::DestructiveUpdate => "DESTRUCTIVE_UPDATE",
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
            // Operational
            Self::NothingToDo => "NOTHING_TO_DO",
            Self::PartiallyClosed => "PARTIALLY_CLOSED",
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
                | Self::DestructiveUpdate
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
            | Self::WriteMismatch
            | Self::NotInitialized
            | Self::AlreadyInitialized => 2,
            // Issue / Operational (3)
            Self::IssueNotFound
            | Self::AmbiguousId
            | Self::IdCollision
            | Self::InvalidId
            | Self::NothingToDo
            | Self::PartiallyClosed => 3,
            // Validation (4)
            Self::ValidationFailed
            | Self::InvalidStatus
            | Self::DestructiveUpdate
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
        let hint = Some(status_hint(provided));

        let context = json!({
            "provided": provided,
            "valid_values": Status::PARSEABLE,
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
            Some("Valid types: task, bug, feature, epic, chore".to_string())
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
            BeadsError::DestructiveFieldShrink {
                id,
                field,
                flag,
                old_chars,
                new_chars,
            } => (
                ErrorCode::DestructiveUpdate,
                Some(json!({
                    "id": id,
                    "field": field,
                    "flag": flag,
                    "old_chars": old_chars,
                    "new_chars": new_chars,
                    "removed_chars": old_chars.saturating_sub(*new_chars),
                    "override_flag": REPLACE_FLAG,
                    // Absent for the title, which has no append channel:
                    // a machine reader must not be handed an alternative
                    // that does not exist for this field.
                    "append_alternative": field_has_append_alternative(field)
                        .then_some("br comments add"),
                })),
            ),
            BeadsError::WriteDidNotLandAsSent {
                id,
                field,
                requested_chars,
                stored_chars,
            } => (
                ErrorCode::WriteMismatch,
                Some(json!({
                    "id": id,
                    "field": field,
                    "requested_chars": requested_chars,
                    "stored_chars": stored_chars,
                    "landed_as_sent": false,
                })),
            ),
            BeadsError::InvalidStatus { status } => (
                ErrorCode::InvalidStatus,
                Some(serde_json::json!({
                    "status": status,
                    "hint": status_did_you_mean(status),
                    "valid_values": Status::PARSEABLE,
                })),
            ),
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
            BeadsError::NothingToDo { skipped } => (
                ErrorCode::NothingToDo,
                Some(skip_context(
                    0,
                    skipped,
                    &format!("all {} issue(s) skipped", skipped.len()),
                )),
            ),
            BeadsError::PartiallyClosed { closed, skipped } => (
                ErrorCode::PartiallyClosed,
                Some(skip_context(
                    *closed,
                    skipped,
                    &format!(
                        "closed {closed} of {}, {} skipped",
                        closed + skipped.len(),
                        skipped.len()
                    ),
                )),
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
        // Invalid status is handled first, ahead of the generic
        // `suggestion()` fallthrough below. `suggestion()` always
        // returns Some for this variant, so the targeted
        // "Did you mean ...?" arm further down was unreachable and the
        // user only ever saw the bare list — including for
        // `--status all`, where the answer is a different flag
        // entirely (`-a/--all`).
        if let BeadsError::InvalidStatus { status } = err {
            return Some(status_hint(status));
        }

        // A refused destructive write: the caller's payload is intact and
        // unwritten, so the hint's whole job is to name both ways forward
        // without making either one the path of least resistance.
        if let BeadsError::DestructiveFieldShrink {
            id,
            field,
            flag,
            old_chars,
            new_chars,
        } = err
        {
            let removed = old_chars.saturating_sub(*new_chars);
            // The append channel is named only for fields that have one.
            // Telling someone retitling an issue to "use br comments add"
            // is advice they cannot follow, and a hint that does not fit
            // the situation is a hint that stops being read.
            let keep_it = if field_has_append_alternative(field) {
                format!(
                    "To ADD to the field without losing what is there, use \
                     'br comments add {id} -f <file>' (append-only, attributed, timestamped) \
                     or re-send {flag} with the existing text included."
                )
            } else {
                format!("To keep what is there, re-send {flag} with the existing text included.")
            };
            return Some(format!(
                "This would remove {removed} of {old_chars} chars from {field}. \
                 Nothing was written. {keep_it} \
                 To replace it anyway, re-run the same command with {REPLACE_FLAG}."
            ));
        }

        // The write already happened, so this hint cannot offer a way to
        // prevent it — only a way to find out what is now in the field.
        if let BeadsError::WriteDidNotLandAsSent {
            id,
            field,
            requested_chars,
            stored_chars,
        } = err
        {
            return Some(format!(
                "bd stored {stored_chars} chars of the {requested_chars} it was handed for \
                 {field}, so the write landed altered. The value bd RECEIVED is what it \
                 compared against, so this is a discrepancy inside bd, not in your shell. \
                 Read the field back in full with 'br show {id} --json' before re-sending, \
                 and do not treat the update as applied."
            ));
        }

        // Otherwise check if BeadsError has a built-in suggestion
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
            // The generic "already closed or not found" sentence used to
            // sit here and contradicted the per-id reason printed two lines
            // above it (bead `beads1-3c8h4`). The hint is now rendered from
            // the same `SkipReason` values as the warning line, so the two
            // cannot disagree.
            BeadsError::NothingToDo { skipped } => Some(skip_hint(skipped)),
            BeadsError::PartiallyClosed { closed, skipped } => Some(format!(
                "{closed} of {} requested issue(s) closed. {}",
                closed + skipped.len(),
                skip_hint(skipped)
            )),
            BeadsError::JsonlParse { line, .. } => Some(format!(
                "Check line {line} of the JSONL file for syntax errors."
            )),
            _ => None,
        }
    }
}

/// The reason discriminators, in the order a hint mentions them:
/// the things the caller must act on first.
const SKIP_CODE_ORDER: [&str; 4] = ["blocked", "not_found", "tombstoned", "already_closed"];

/// Machine-readable account of a partly- or wholly-skipped batch.
///
/// Everything a caller needs is keyed, not prose: `skipped[].reason` is a
/// stable discriminator (`blocked`, `already_closed`, `tombstoned`,
/// `not_found`), `skipped[].blockers` names the blockers, and
/// `outstanding` is the subset the caller still has to do something
/// about. Nothing here requires string-matching a sentence.
///
/// `reason` (the summary string) is retained because it is what the
/// previous `NOTHING_TO_DO` context carried, and dropping a field is a
/// break for a consumer that reads it.
#[must_use]
pub fn skip_context(closed: usize, skipped: &[SkippedTarget], summary: &str) -> Value {
    let entries: Vec<Value> = skipped
        .iter()
        .map(|s| {
            let mut entry = json!({
                "id": s.id,
                "reason": s.reason.code(),
                "detail": s.reason.describe(),
                "end_state_reached": s.reason.end_state_reached(),
            });
            let blockers = s.reason.blockers();
            if !blockers.is_empty() {
                entry["blockers"] = json!(blockers);
            }
            entry
        })
        .collect();

    let mut reasons: Vec<&str> = Vec::new();
    for skip in skipped {
        let code = skip.reason.code();
        if !reasons.contains(&code) {
            reasons.push(code);
        }
    }

    let outstanding: Vec<&str> = skipped
        .iter()
        .filter(|s| !s.reason.end_state_reached())
        .map(|s| s.id.as_str())
        .collect();

    json!({
        "reason": summary,
        "requested_count": closed + skipped.len(),
        "closed_count": closed,
        "skipped_count": skipped.len(),
        "skip_reasons": reasons,
        "outstanding": outstanding,
        "skipped": entries,
    })
}

/// The hint for a skipped batch, rendered from the same
/// [`crate::error::SkipReason`] values as the `Warning: Skipped ...` lines.
///
/// One sentence per blocked id (each has its own blockers and its own
/// remedy), one sentence per other reason group. Long batches are
/// truncated with a pointer at `context.skipped`, which is complete.
#[must_use]
pub fn skip_hint(skipped: &[SkippedTarget]) -> String {
    /// Beyond this many ids in one group the hint stops enumerating.
    const MAX_LISTED: usize = 3;

    let mut sentences: Vec<String> = Vec::new();

    for code in SKIP_CODE_ORDER {
        let group: Vec<&SkippedTarget> =
            skipped.iter().filter(|s| s.reason.code() == code).collect();
        if group.is_empty() {
            continue;
        }

        if code == "blocked" {
            // Blockers differ per id, so each blocked id gets its own
            // sentence naming ITS blockers — the whole point of the fix is
            // that the specific reason survives to the caller.
            for skip in group.iter().take(MAX_LISTED) {
                sentences.push(format!(
                    "{} was not closed: {}. {}",
                    skip.id,
                    skip.reason.describe(),
                    skip.reason.remedy(std::slice::from_ref(&skip.id))
                ));
            }
            if group.len() > MAX_LISTED {
                sentences.push(format!(
                    "{} further blocked issue(s) were skipped; see context.skipped for all of them.",
                    group.len() - MAX_LISTED
                ));
            }
            continue;
        }

        let ids: Vec<String> = group.iter().map(|s| s.id.clone()).collect();
        let listed: Vec<String> = ids.iter().take(MAX_LISTED).cloned().collect();
        let and_more = if ids.len() > listed.len() {
            format!(
                " (and {} more; see context.skipped)",
                ids.len() - listed.len()
            )
        } else {
            String::new()
        };
        // `describe`/`remedy` come from the group's own reason, so the
        // sentence cannot claim a reason that was not the reason.
        let reason = &group[0].reason;
        sentences.push(format!(
            "{}{and_more}: {}. {}",
            listed.join(", "),
            reason.describe(),
            reason.remedy(&listed)
        ));
    }

    if sentences.is_empty() {
        // No skips recorded: say that rather than inventing a reason.
        return "No issues changed state.".to_string();
    }
    sentences.join(" ")
}

/// The targeted part of an invalid-status hint, if there is one: a
/// "did you mean" for a near miss or a synonym, or the pointer at
/// `-a/--all` for the specific mistake of `--status all`. `None` when
/// nothing about the input suggests a particular intent.
fn status_did_you_mean(provided: &str) -> Option<String> {
    if provided.trim().eq_ignore_ascii_case("all") {
        return Some(STATUS_ALL_HINT.to_string());
    }
    detect_status_intent(provided).map(|detected| format!("Did you mean --status {detected}?"))
}

/// Whether `br comments add` is a usable alternative for this field.
///
/// An unrecognised name is treated as having one: a field added later is far
/// more likely to be prose than to be another one-line label, and the cost of
/// being wrong is one extra sentence in a hint, not a lost payload.
fn field_has_append_alternative(field: &str) -> bool {
    TextField::from_name(field).is_none_or(TextField::has_append_alternative)
}

/// Full hint for an unparseable status: the targeted suggestion (when
/// the input points at one) followed by the authoritative list of what
/// `Status::from_str` accepts.
fn status_hint(provided: &str) -> String {
    match status_did_you_mean(provided) {
        Some(targeted) => format!("{targeted} {VALID_STATUSES_HINT}"),
        None => VALID_STATUSES_HINT.to_string(),
    }
}

// === Precomputed Valid Values (O(1) lookup) ===

/// Valid status values — derived from `Status::PARSEABLE` so
/// intent detection and the user-facing hint can never disagree with
/// what `Status::from_str` actually accepts.
static VALID_STATUSES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| Status::PARSEABLE.into_iter().collect());

/// Valid issue type values (matching bd conformance).
static VALID_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["task", "bug", "feature", "epic", "chore"]
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

    /// `--status all` is the natural way to reach for `-a/--all`, so
    /// the hint must point at that flag rather than only reciting the
    /// status list (bead `beads1-17zqr`). Asserted on both surfaces:
    /// the primary `hint` line and the machine-readable `context.hint`.
    #[test]
    fn status_all_points_at_the_all_flag() {
        let err = StructuredError::invalid_status("all");
        let hint = err.hint.as_deref().expect("hint");
        assert!(
            hint.contains("--all"),
            "'--status all' must suggest -a/--all: {hint}"
        );
        assert!(
            hint.contains("Valid statuses:"),
            "the valid-status list should still follow the suggestion: {hint}"
        );

        let from_beads_error = StructuredError::from_error(&BeadsError::InvalidStatus {
            status: "all".to_string(),
        });
        let hint = from_beads_error.hint.as_deref().expect("hint");
        assert!(hint.contains("--all"), "BeadsError path must agree: {hint}");
        let ctx_hint = from_beads_error.context.as_ref().expect("context")["hint"]
            .as_str()
            .expect("context.hint should be set for 'all'");
        assert!(ctx_hint.contains("--all"), "context.hint: {ctx_hint}");
    }

    /// A near-miss / synonym now reaches the *primary* hint line, not
    /// just `context.hint`: `generate_hint` used to return the generic
    /// `suggestion()` first, making the targeted arm unreachable.
    #[test]
    fn did_you_mean_reaches_the_primary_hint() {
        let err = StructuredError::from_error(&BeadsError::InvalidStatus {
            status: "wip".to_string(),
        });
        let hint = err.hint.as_deref().expect("hint");
        assert!(
            hint.contains("Did you mean --status in_progress?"),
            "targeted suggestion should lead the hint: {hint}"
        );
        assert!(hint.contains("Valid statuses:"), "{hint}");
    }

    /// The `valid_values` context array is what an agent parses to
    /// retry; it must be the parser's own set.
    #[test]
    fn invalid_status_context_lists_parseable_statuses() {
        let err = StructuredError::from_error(&BeadsError::InvalidStatus {
            status: "nope".to_string(),
        });
        let values = err.context.as_ref().expect("context")["valid_values"]
            .as_array()
            .expect("valid_values array")
            .iter()
            .map(|v| v.as_str().expect("string").to_string())
            .collect::<Vec<_>>();
        assert_eq!(values, Status::PARSEABLE.to_vec());
    }

    /// A refused destructive write has to hand back everything the caller
    /// needs to proceed, in the same envelope shape as any other refusal.
    #[test]
    fn destructive_shrink_reports_sizes_and_the_override_flag() {
        let err = StructuredError::from_error(&BeadsError::DestructiveFieldShrink {
            id: "np-3pp".to_string(),
            field: "notes".to_string(),
            flag: "--notes".to_string(),
            old_chars: 62,
            new_chars: 27,
        });
        assert_eq!(err.code, ErrorCode::DestructiveUpdate);
        assert_eq!(err.code.exit_code(), 4);
        let ctx = err.context.as_ref().expect("context");
        assert_eq!(ctx["old_chars"], 62);
        assert_eq!(ctx["new_chars"], 27);
        assert_eq!(ctx["removed_chars"], 35);
        assert_eq!(ctx["override_flag"], "--replace");
        let hint = err.hint.as_deref().expect("hint");
        assert!(hint.contains("--replace"), "{hint}");
        assert!(hint.contains("br comments add"), "{hint}");
        assert!(hint.contains("Nothing was written"), "{hint}");
    }

    /// A title has no append channel, so neither the hint nor the machine
    /// context may name one. Advice a caller cannot follow is how a hint
    /// teaches people to skip hints.
    #[test]
    fn title_refusal_does_not_advertise_an_append_channel() {
        let err = StructuredError::from_error(&BeadsError::DestructiveFieldShrink {
            id: "np-3pp".to_string(),
            field: "title".to_string(),
            flag: "--title".to_string(),
            old_chars: 23,
            new_chars: 5,
        });
        let hint = err.hint.as_deref().expect("hint");
        assert!(
            !hint.contains("br comments add"),
            "a title cannot be appended to: {hint}"
        );
        assert!(hint.contains("--replace"), "{hint}");
        assert!(hint.contains("re-send --title"), "{hint}");
        assert!(hint.contains("Nothing was written"), "{hint}");
        let ctx = err.context.as_ref().expect("context");
        assert_eq!(
            ctx["append_alternative"],
            serde_json::Value::Null,
            "machine readers must not be handed a channel this field lacks"
        );
    }

    /// Prose fields keep the append advice — that is the whole point of the
    /// hint, and the title carve-out must not quietly swallow it.
    #[test]
    fn prose_field_refusals_all_name_the_append_channel() {
        for field in ["description", "design", "acceptance_criteria", "notes"] {
            let err = StructuredError::from_error(&BeadsError::DestructiveFieldShrink {
                id: "np-3pp".to_string(),
                field: field.to_string(),
                flag: format!("--{field}"),
                old_chars: 62,
                new_chars: 27,
            });
            let hint = err.hint.as_deref().expect("hint");
            assert!(hint.contains("br comments add"), "{field}: {hint}");
            let ctx = err.context.as_ref().expect("context");
            assert_eq!(ctx["append_alternative"], "br comments add", "{field}");
        }
    }

    /// An unknown field name must fail toward MORE advice, not less: a field
    /// added later is prose until someone says otherwise.
    #[test]
    fn unknown_field_keeps_the_append_advice() {
        let err = StructuredError::from_error(&BeadsError::DestructiveFieldShrink {
            id: "np-3pp".to_string(),
            field: "some_future_field".to_string(),
            flag: "--some-future-field".to_string(),
            old_chars: 62,
            new_chars: 27,
        });
        let hint = err.hint.as_deref().expect("hint");
        assert!(hint.contains("br comments add"), "{hint}");
    }

    /// A write that did not land as sent is a DIFFERENT failure from the
    /// refusal, and automation has to be able to tell them apart: the
    /// refusal prevented a loss, this one is reporting an accomplished one.
    #[test]
    fn write_mismatch_is_separable_from_the_refusal() {
        let err = StructuredError::from_error(&BeadsError::WriteDidNotLandAsSent {
            id: "np-3pp".to_string(),
            field: "notes".to_string(),
            requested_chars: 16,
            stored_chars: 5,
        });
        assert_eq!(err.code, ErrorCode::WriteMismatch);
        assert_eq!(err.code.as_str(), "WRITE_MISMATCH");
        assert_eq!(err.code.exit_code(), 2);
        assert_ne!(
            ErrorCode::WriteMismatch.exit_code(),
            ErrorCode::DestructiveUpdate.exit_code(),
            "the two must be distinguishable by exit code alone"
        );
        let ctx = err.context.as_ref().expect("context");
        assert_eq!(ctx["requested_chars"], 16);
        assert_eq!(ctx["stored_chars"], 5);
        // A boolean, not only a human string: this is what a script reads.
        assert_eq!(ctx["landed_as_sent"], serde_json::Value::Bool(false));
    }

    /// The hint must not blame the caller's shell for something bd measured
    /// on its own side of the boundary.
    #[test]
    fn write_mismatch_hint_locates_the_discrepancy_inside_bd() {
        let err = StructuredError::from_error(&BeadsError::WriteDidNotLandAsSent {
            id: "np-3pp".to_string(),
            field: "notes".to_string(),
            requested_chars: 16,
            stored_chars: 5,
        });
        let hint = err.hint.as_deref().expect("hint");
        assert!(hint.contains("inside bd"), "{hint}");
        assert!(hint.contains("br show np-3pp --json"), "{hint}");
        assert!(
            hint.contains("do not treat the update as applied"),
            "{hint}"
        );
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

    // =========================================================================
    // Skip reporting (bead beads1-3c8h4): the hint must say what actually
    // happened, and a caller must be able to tell blocked from
    // already-closed without reading English.
    // =========================================================================

    use crate::error::SkipReason;

    /// The exact sentence the old hint always printed. Kept as a literal so
    /// that reintroducing it anywhere fails a test.
    const OLD_WRONG_HINT: &str = "All specified issues were already closed or not found.";

    fn blocked(id: &str, blockers: &[&str]) -> SkippedTarget {
        SkippedTarget::new(
            id,
            SkipReason::Blocked {
                blockers: blockers.iter().map(|b| (*b).to_string()).collect(),
            },
        )
    }

    fn already(id: &str) -> SkippedTarget {
        SkippedTarget::new(
            id,
            SkipReason::AlreadyTerminal {
                status: Status::Closed,
            },
        )
    }

    #[test]
    fn skip_hint_for_a_blocked_id_names_the_blocker_and_says_what_to_do() {
        let skipped = vec![blocked("t-1euci", &["t-21y5o:open"])];
        let hint = skip_hint(&skipped);

        assert!(hint.contains("t-1euci"), "names the skipped id: {hint}");
        assert!(hint.contains("t-21y5o"), "names the blocker: {hint}");
        assert!(hint.contains("--force"), "offers the override: {hint}");
        // The blocker is stored as `id:status`; a suggested command has to
        // carry the bare id or it cannot be pasted.
        assert!(
            hint.contains("br close t-21y5o'"),
            "suggests a runnable command: {hint}"
        );
    }

    #[test]
    fn skip_hint_for_a_blocked_id_does_not_claim_it_was_already_closed() {
        let hint = skip_hint(&[blocked("t-1euci", &["t-21y5o:open"])]);
        assert!(
            !hint.contains("already closed"),
            "blocked is not already closed: {hint}"
        );
        assert!(
            !hint.contains("not found"),
            "blocked is not not-found: {hint}"
        );
        assert_ne!(hint, OLD_WRONG_HINT);
    }

    #[test]
    fn skip_hint_agrees_with_the_warning_line_for_every_reason() {
        // Both surfaces render from SkipReason, so the detail printed as
        // `Warning: Skipped <id>: <detail>` must appear in the hint.
        let cases = vec![
            blocked("t-a", &["t-b:open"]),
            already("t-c"),
            SkippedTarget::new(
                "t-d",
                SkipReason::AlreadyTerminal {
                    status: Status::Tombstone,
                },
            ),
            SkippedTarget::new("t-e", SkipReason::NotFound),
        ];
        for target in &cases {
            let hint = skip_hint(std::slice::from_ref(target));
            let warning_detail = target.reason.describe();
            assert!(
                hint.contains(&warning_detail),
                "hint {hint:?} must contain the warning detail {warning_detail:?}"
            );
        }
    }

    #[test]
    fn skip_hint_keeps_mixed_reasons_apart() {
        let skipped = vec![
            blocked("t-a", &["t-b:open"]),
            already("t-c"),
            SkippedTarget::new(
                "t-d",
                SkipReason::AlreadyTerminal {
                    status: Status::Tombstone,
                },
            ),
        ];
        let hint = skip_hint(&skipped);
        assert!(
            hint.contains("t-a") && hint.contains("blocked by"),
            "{hint}"
        );
        assert!(hint.contains("t-c") && hint.contains("br reopen"), "{hint}");
        assert!(hint.contains("t-d") && hint.contains("tombstone"), "{hint}");
        // A tombstone must never be advertised as reopenable work: it is a
        // deleted bead, and `br reopen` on it is not the remedy.
        assert!(
            !hint.contains("br reopen t-d"),
            "tombstone advice must not offer reopen: {hint}"
        );
    }

    #[test]
    fn skip_hint_stops_enumerating_long_groups_but_points_at_the_full_list() {
        let skipped: Vec<SkippedTarget> = (0..7).map(|i| already(&format!("t-{i}"))).collect();
        let hint = skip_hint(&skipped);
        assert!(hint.contains("t-0"), "lists the first few: {hint}");
        assert!(hint.contains("and 4 more"), "counts the rest: {hint}");
        assert!(
            hint.contains("context.skipped"),
            "points at the complete list: {hint}"
        );
        // Truncation must not silently drop ids from the machine-readable
        // side: context.skipped carries all 7.
        let ctx = skip_context(0, &skipped, "all 7 issue(s) skipped");
        assert_eq!(ctx["skipped"].as_array().expect("array").len(), 7);
    }

    #[test]
    fn skip_hint_with_many_blocked_ids_names_the_first_few_and_counts_the_rest() {
        let skipped: Vec<SkippedTarget> = (0..5)
            .map(|i| blocked(&format!("t-{i}"), &["t-b:open"]))
            .collect();
        let hint = skip_hint(&skipped);
        assert!(hint.contains("t-0 was not closed"), "{hint}");
        assert!(hint.contains("2 further blocked issue(s)"), "{hint}");
        assert!(hint.contains("context.skipped"), "{hint}");
    }

    #[test]
    fn skip_hint_with_no_skips_does_not_invent_a_reason() {
        // Guards the fallback: a hint must never assert something the code
        // did not observe, which is the whole of bead beads1-3c8h4.
        let hint = skip_hint(&[]);
        assert_eq!(hint, "No issues changed state.");
        assert!(!hint.contains("already closed"));
        assert!(!hint.contains("blocked"));
    }

    #[test]
    fn skip_context_lets_a_caller_discriminate_without_reading_prose() {
        let skipped = vec![blocked("t-a", &["t-b:open"]), already("t-c")];
        let ctx = skip_context(1, &skipped, "closed 1 of 3, 2 skipped");

        assert_eq!(ctx["requested_count"], 3);
        assert_eq!(ctx["closed_count"], 1);
        assert_eq!(ctx["skipped_count"], 2);
        assert_eq!(ctx["skipped"][0]["reason"], "blocked");
        assert_eq!(ctx["skipped"][0]["blockers"][0], "t-b:open");
        assert_eq!(ctx["skipped"][0]["end_state_reached"], false);
        assert_eq!(ctx["skipped"][1]["reason"], "already_closed");
        assert_eq!(ctx["skipped"][1]["end_state_reached"], true);
        assert!(ctx["skipped"][1].get("blockers").is_none());
        // `outstanding` is the actionable subset: what the caller still has
        // to do something about, already-closed excluded.
        assert_eq!(ctx["outstanding"], json!(["t-a"]));
        assert_eq!(ctx["skip_reasons"], json!(["blocked", "already_closed"]));
        // Retained for consumers of the previous NOTHING_TO_DO context.
        assert_eq!(ctx["reason"], "closed 1 of 3, 2 skipped");
    }

    #[test]
    fn nothing_to_do_reports_the_computed_reason_not_a_generic_sentence() {
        let err = BeadsError::NothingToDo {
            skipped: vec![blocked("t-1euci", &["t-21y5o:open"])],
        };
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::NothingToDo);
        assert_eq!(structured.code.exit_code(), 3);
        let hint = structured.hint.as_deref().expect("hint");
        assert_ne!(hint, OLD_WRONG_HINT, "the generic sentence is back");
        assert!(hint.contains("t-21y5o"), "{hint}");
        let ctx = structured.context.expect("context");
        assert_eq!(ctx["skipped"][0]["reason"], "blocked");
        assert_eq!(ctx["outstanding"], json!(["t-1euci"]));
    }

    #[test]
    fn partially_closed_is_a_distinct_code_that_still_fails() {
        let err = BeadsError::PartiallyClosed {
            closed: 2,
            skipped: vec![blocked("t-a", &["t-b:open"])],
        };
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::PartiallyClosed);
        assert_eq!(structured.code.as_str(), "PARTIALLY_CLOSED");
        assert_ne!(structured.code, ErrorCode::NothingToDo);
        assert_eq!(structured.code.exit_code(), 3);
        assert!(!structured.retryable);
        assert!(structured.message.contains("Closed 2 of 3"));
        let ctx = structured.context.expect("context");
        assert_eq!(ctx["closed_count"], 2);
        assert_eq!(ctx["outstanding"], json!(["t-a"]));
    }
}
