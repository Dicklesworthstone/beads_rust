//! Error types and handling for `beads_rust`.
//!
//! This module provides structured errors that match the classic bd
//! behavior for JSON error output compatibility.
//!
//! # Design
//!
//! - Uses `thiserror` for derive-based error types
//! - Provides recovery hints for user-facing errors
//! - Matches bd's exit code conventions
//! - Provides structured JSON output for AI coding agents

mod context;
mod structured;

pub use context::{OptionExt, ResultExt};
pub(crate) use structured::levenshtein_distance;
pub use structured::{ErrorCode, StructuredError};

use std::path::PathBuf;
use thiserror::Error;

/// Primary error type for `beads_rust` operations.
///
/// Design: Structured variants for common cases.
#[derive(Error, Debug)]
pub enum BeadsError {
    // === Storage Errors ===
    /// Database file not found at the specified path.
    #[error("Database not found at '{path}'")]
    DatabaseNotFound { path: PathBuf },

    /// Database is locked by another process.
    #[error("Database is locked: {path}")]
    DatabaseLocked { path: PathBuf },

    /// Database schema version doesn't match expected.
    #[error("Schema version mismatch: expected {expected}, found {found}")]
    SchemaMismatch { expected: i32, found: i32 },

    /// `SQLite` database error.
    #[error("Database error: {0}")]
    Database(#[from] fsqlite_error::FrankenError),

    // === Issue Errors ===
    /// Issue with the specified ID was not found.
    #[error("Issue not found: {id}")]
    IssueNotFound { id: String },

    /// Attempted to create an issue with an ID that already exists.
    #[error("Issue ID collision: {id}")]
    IdCollision { id: String },

    /// Partial ID matches multiple issues.
    #[error("Ambiguous ID '{partial}': matches {matches:?}")]
    AmbiguousId {
        partial: String,
        matches: Vec<String>,
    },

    /// Issue ID format is invalid.
    #[error("Invalid issue ID format: {id}")]
    InvalidId { id: String },

    // === Validation Errors ===
    /// Field validation failed.
    #[error("Validation failed: {field}: {reason}")]
    Validation { field: String, reason: String },

    /// Field validation failed for a mistake the docs make easy to commit;
    /// carries the actionable fix so text and JSON output both show it.
    #[error("Validation failed: {field}: {reason}")]
    ValidationWithHint {
        field: String,
        reason: String,
        hint: String,
    },

    /// Multiple validation errors occurred.
    #[error("Validation errors: {errors:?}")]
    ValidationErrors { errors: Vec<ValidationError> },

    /// Invalid status value.
    #[error("Invalid status: {status}")]
    InvalidStatus { status: String },

    /// Invalid issue type value.
    #[error("Invalid issue type: {issue_type}")]
    InvalidType { issue_type: String },

    /// Priority out of valid range (0-4).
    #[error("Priority must be 0-4, got: {priority}")]
    InvalidPriority { priority: String },

    // === JSONL Errors ===
    /// Failed to parse a line in the JSONL file.
    #[error("JSONL parse error at line {line}: {reason}")]
    JsonlParse { line: usize, reason: String },

    /// Issue prefix doesn't match expected prefix.
    #[error("Prefix mismatch: expected '{expected}', found '{found}'")]
    PrefixMismatch { expected: String, found: String },

    /// Import found conflicting issues.
    #[error("Import collision: {count} issues have conflicting content")]
    ImportCollision { count: usize },

    /// Conflict detected between local and external changes.
    #[error("Sync conflict: {message}")]
    SyncConflict { message: String },

    /// A mutation committed, but the process lost authority to witness the
    /// committed database inode before it could report ordinary success.
    ///
    /// Callers must not retry automatically: the mutation body may already be
    /// durable on the displaced inode.
    #[error(
        "{operation} committed, but database authority changed before it could be witnessed; reconcile committed state before retrying: {source}"
    )]
    CommittedStateUnwitnessed {
        operation: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A JSONL exchange completed, but the displaced generation did not match
    /// the exact source generation the exporting session had retained.
    ///
    /// The publisher preserves both names instead of risking a destructive
    /// rollback across another actor's write. Callers must reconcile the
    /// primary and recovery paths manually before retrying.
    #[error(
        "JSONL publication conflict at '{output_path}': {message}; the displaced generation is preserved at '{recovery_path}'"
    )]
    JsonlPublicationConflict {
        output_path: PathBuf,
        recovery_path: PathBuf,
        message: String,
    },

    /// The new JSONL generation reached its destination name and was verified,
    /// but the containing directory could not be synced.
    ///
    /// The operation is committed from the namespace's point of view but is
    /// not certified power-loss durable. Automatic retry could overwrite a
    /// newer generation, so callers must reconcile first.
    #[error(
        "JSONL generation {content_sha256} was published at '{output_path}', but its directory durability could not be certified; recovery copy: {recovery_path:?}: {source}"
    )]
    JsonlPublishedButNotDurable {
        output_path: PathBuf,
        recovery_path: Option<PathBuf>,
        content_sha256: String,
        #[source]
        source: std::io::Error,
    },

    /// The namespace-changing publication syscall completed, but the process
    /// could not certify the resulting target generation.
    ///
    /// A displaced generation, when any, remains at `recovery_path`. Callers
    /// must inspect both paths and must not automatically retry.
    #[error(
        "JSONL publication changed '{output_path}', but the published generation could not be certified; recovery copy: {recovery_path:?}: {source}"
    )]
    JsonlPublishedButUnwitnessed {
        output_path: PathBuf,
        recovery_path: Option<PathBuf>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The command's primary state was already committed and finalized, but a
    /// requested or required auxiliary artifact could not be published.
    ///
    /// Retrying the whole command is unsafe because its primary mutation may
    /// no longer be idempotent. Callers should repair only the named artifact.
    #[error(
        "{operation} committed its primary state at '{primary_path}', but failed to publish auxiliary artifact '{artifact_path}': {source}"
    )]
    CommittedArtifactFailure {
        operation: String,
        primary_path: PathBuf,
        artifact_path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // === Dependency Errors ===
    /// Adding the dependency would create a cycle.
    #[error("Cycle detected in dependencies: {path}")]
    DependencyCycle { path: String },

    /// Cannot delete an issue that has dependents.
    #[error("Cannot delete: {id} has {count} dependents")]
    HasDependents { id: String, count: usize },

    /// Self-referential dependency.
    #[error("Issue cannot depend on itself: {id}")]
    SelfDependency { id: String },

    /// Dependency target not found.
    #[error("Dependency target not found: {id}")]
    DependencyNotFound { id: String },

    /// Duplicate dependency.
    #[error("Dependency already exists: {from} -> {to}")]
    DuplicateDependency { from: String, to: String },

    // === Configuration Errors ===
    /// Configuration file error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// External command failed or returned unusable output.
    #[error("External command failed: {command}: {reason}")]
    ExternalCommand { command: String, reason: String },

    /// Self-update or upgrade operation failed.
    #[error("Upgrade failed: {reason}")]
    Upgrade { reason: String },

    /// Internal consistency check failed.
    #[error("Internal error: {message}")]
    Internal { message: String },

    /// Beads workspace not initialized.
    #[error("Beads not initialized: run 'br init' first")]
    NotInitialized,

    /// Already initialized.
    #[error("Already initialized at '{path}'")]
    AlreadyInitialized { path: PathBuf },

    // === I/O Errors ===
    /// File system I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML parsing error.
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),

    // === Wrapped errors (for gradual migration) ===
    /// Error with additional context.
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // === Operational Errors ===
    /// Operation refused because cooperative shutdown has already been requested.
    #[error("Shutdown requested")]
    ShuttingDown,

    /// All requested items were skipped (already closed, not found, etc.).
    #[error("Nothing to do: {reason}")]
    NothingToDo { reason: String },

    /// Some requested items were applied and the rest were skipped.
    ///
    /// A partially applied batch must never report success. The caller asked
    /// for N transitions and got fewer, so the exit status has to carry that:
    /// otherwise a skip is visible only as a warning on stderr, and a caller
    /// that branches on `$?` — or that follows `docs/agent/ERRORS.md` and
    /// parses stdout because the exit code was `0` — reads the partial batch
    /// as a complete success.
    #[error("Partially applied: {closed} closed, {skipped} skipped — {summary}")]
    CloseIncomplete {
        closed: usize,
        skipped: usize,
        summary: String,
    },

    // === Policy Errors ===
    /// One or more closure-time policy gates fired.
    ///
    /// Display format intentionally repeats the gate that fired and a
    /// short explanation so terminal output stays readable; structured
    /// callers should serialise the inner [`crate::close_policy::PolicyViolation`]s
    /// via [`StructuredError::context`].
    #[error("Policy violation closing {issue_id}: {summary}")]
    PolicyViolation {
        issue_id: String,
        summary: String,
        violations: Vec<crate::close_policy::PolicyViolation>,
    },

    /// A status transition would exceed an atomically enforced workflow
    /// capacity or cross-queue admission threshold (GitHub #384).
    #[error("{violation}")]
    WorkflowCapacityExceeded {
        violation: Box<crate::close_policy::WorkflowCapacityViolation>,
    },
}

impl BeadsError {
    /// Route a schema-version mismatch into the reviewed migration workflow.
    ///
    /// Ordinary commands never migrate an existing tracker database in place;
    /// both the startup pending-merge gate and the read-only fast-open
    /// writable fallback wrap the raw [`Self::SchemaMismatch`] with this
    /// operator guidance so every refusal names the same explicit,
    /// receipt-bound path.
    #[must_use]
    pub fn reviewed_schema_migration_required(self) -> Self {
        Self::WithContext {
            context: "ordinary commands never migrate an existing tracker database; run \
                      `br doctor migrate-schema plan` and review its receipt before applying the \
                      explicit migration"
                .to_string(),
            source: Box::new(self),
        }
    }

    /// Returns true if the error is transient and can be retried.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Database(e) => e.is_transient(),
            Self::ShuttingDown => true,
            Self::Io(e) => {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                )
            }
            _ => false,
        }
    }
}

/// A single field validation error.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// The field that failed validation.
    pub field: String,
    /// The reason for the validation failure.
    pub message: String,
}

impl ValidationError {
    /// Create a new validation error.
    #[must_use]
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

impl BeadsError {
    /// Can the user fix this without code changes?
    #[must_use]
    pub const fn is_user_recoverable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseNotFound { .. }
                | Self::NotInitialized
                | Self::IssueNotFound { .. }
                | Self::Validation { .. }
                | Self::ValidationWithHint { .. }
                | Self::InvalidStatus { .. }
                | Self::InvalidType { .. }
                | Self::InvalidPriority { .. }
                | Self::PrefixMismatch { .. }
                | Self::AmbiguousId { .. }
                | Self::PolicyViolation { .. }
                | Self::WorkflowCapacityExceeded { .. }
        )
    }

    /// Should we suggest re-running with --force?
    #[must_use]
    pub const fn suggests_force(&self) -> bool {
        matches!(
            self,
            Self::HasDependents { .. }
                | Self::ImportCollision { .. }
                | Self::AlreadyInitialized { .. }
        )
    }

    /// Whether the error proves that the command's primary mutation or
    /// namespace publication may already be committed.
    ///
    /// Deferred-recovery callers must never restore an older database family
    /// in response to one of these errors.
    #[must_use]
    pub fn primary_mutation_committed(&self) -> bool {
        match self {
            Self::CommittedStateUnwitnessed { .. }
            | Self::JsonlPublicationConflict { .. }
            | Self::JsonlPublishedButNotDurable { .. }
            | Self::JsonlPublishedButUnwitnessed { .. }
            | Self::CommittedArtifactFailure { .. } => true,
            Self::WithContext { source, .. } => source
                .downcast_ref::<Self>()
                .is_some_and(Self::primary_mutation_committed),
            _ => false,
        }
    }

    /// Human-friendly suggestion for fixing this error.
    #[must_use]
    pub const fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::NotInitialized => Some("Run: br init"),
            Self::DatabaseNotFound { .. } => Some("Check path or run: br init"),
            Self::AmbiguousId { .. } => Some("Provide more characters of the ID"),
            Self::HasDependents { .. } => Some("Use --force or --cascade to delete anyway"),
            Self::ImportCollision { .. } => Some("Use --force to overwrite or resolve manually"),
            Self::DependencyCycle { .. } => Some(
                "Remove one dependency to break the cycle. Note: epic containment \
                 participates in blocking cycles — depending on an epic implies \
                 depending on its entire subtree, so the cycle may traverse \
                 parent-child edges that the message does not list (see \
                 docs/CLI_REFERENCE.md, `dep add`)",
            ),
            Self::SelfDependency { .. } => Some("An issue cannot depend on itself"),
            Self::AlreadyInitialized { .. } => Some("Use --force to reinitialize"),
            Self::InvalidPriority { .. } => Some(
                "Priority must be 0-4 or P0-P4 (0=critical, 4=backlog); list filters also accept ranges and comma lists such as 0-1 or 0,2.",
            ),
            Self::InvalidStatus { .. } => Some(
                "Valid statuses: open, in_progress, blocked, deferred, draft, closed, tombstone, pinned",
            ),
            Self::InvalidType { .. } => {
                Some("Valid types: task, bug, feature, epic, chore, docs, question")
            }
            Self::PolicyViolation { .. } => Some(
                "Fix the violation(s) above, or pass --bypass-policy --bypass-reason \"<text>\" if your project's policy.yaml allows bypass.",
            ),
            Self::WorkflowCapacityExceeded { .. } => Some(
                "Drain the named queue before admitting fresh work; inspect it with `br list --status <status>`.",
            ),
            Self::CommittedStateUnwitnessed { .. } => Some(
                "Do not retry automatically. Reconcile the committed database state and authority first.",
            ),
            Self::JsonlPublicationConflict { .. }
            | Self::JsonlPublishedButNotDurable { .. }
            | Self::JsonlPublishedButUnwitnessed { .. } => Some(
                "Do not retry automatically. Inspect the primary and recovery JSONL generations, reconcile them, then run doctor.",
            ),
            Self::CommittedArtifactFailure { .. } => Some(
                "Do not retry the primary command. Repair only the named auxiliary artifact or run doctor.",
            ),
            _ => None,
        }
    }

    /// Get the exit code for this error.
    ///
    /// Delegates to [`ErrorCode::exit_code()`] via [`StructuredError`] for
    /// consistent, categorized exit codes (1–8).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        StructuredError::from_error(self).code.exit_code()
    }

    /// Create a validation error for a specific field.
    #[must_use]
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Create a validation error that carries an actionable hint.
    #[must_use]
    pub fn validation_with_hint(
        field: impl Into<String>,
        reason: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::ValidationWithHint {
            field: field.into(),
            reason: reason.into(),
            hint: hint.into(),
        }
    }

    /// Create an external command failure.
    #[must_use]
    pub fn external_command(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ExternalCommand {
            command: command.into(),
            reason: reason.into(),
        }
    }

    /// Create a self-update failure.
    #[must_use]
    pub fn upgrade(reason: impl Into<String>) -> Self {
        Self::Upgrade {
            reason: reason.into(),
        }
    }

    /// Create an internal consistency error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// Create from multiple validation errors.
    #[must_use]
    pub fn from_validation_errors(errors: Vec<ValidationError>) -> Self {
        if errors.is_empty() {
            Self::ValidationErrors { errors }
        } else if errors.len() == 1 {
            let err = &errors[0];
            Self::Validation {
                field: err.field.clone(),
                reason: err.message.clone(),
            }
        } else {
            Self::ValidationErrors { errors }
        }
    }
}

/// Result type using `BeadsError`.
pub type Result<T> = std::result::Result<T, BeadsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = BeadsError::IssueNotFound {
            id: "bd-abc123".to_string(),
        };
        assert_eq!(err.to_string(), "Issue not found: bd-abc123");
    }

    #[test]
    fn test_validation_error() {
        let err = BeadsError::validation("title", "cannot be empty");
        assert_eq!(err.to_string(), "Validation failed: title: cannot be empty");
    }

    #[test]
    fn test_external_command_uses_io_error_code() {
        let err = BeadsError::external_command("git", "failed to resolve ref");
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::IoError);
        assert_eq!(err.exit_code(), 8);
    }

    #[test]
    fn test_internal_uses_internal_error_code() {
        let err = BeadsError::internal("routed command produced mismatched counts");
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::InternalError);
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn test_upgrade_uses_io_error_code_with_context() {
        let err = BeadsError::upgrade("network timeout");
        let structured = StructuredError::from_error(&err);

        assert_eq!(structured.code, ErrorCode::IoError);
        assert_eq!(err.exit_code(), 8);
        let context = structured.context.expect("upgrade context");
        assert_eq!(
            context["operation"],
            serde_json::Value::String("upgrade".to_string())
        );
    }

    #[test]
    fn test_user_recoverable() {
        let recoverable = BeadsError::NotInitialized;
        assert!(recoverable.is_user_recoverable());

        let not_recoverable =
            BeadsError::Database(fsqlite_error::FrankenError::Internal("test".to_string()));
        assert!(!not_recoverable.is_user_recoverable());
    }

    #[test]
    fn test_suggestion() {
        let err = BeadsError::NotInitialized;
        assert_eq!(err.suggestion(), Some("Run: br init"));

        let err = BeadsError::AmbiguousId {
            partial: "bd-a".to_string(),
            matches: vec!["bd-abc".to_string(), "bd-abd".to_string()],
        };
        assert_eq!(err.suggestion(), Some("Provide more characters of the ID"));

        let err = BeadsError::InvalidStatus {
            status: "dra".to_string(),
        };
        assert_eq!(
            err.suggestion(),
            Some(
                "Valid statuses: open, in_progress, blocked, deferred, draft, closed, tombstone, pinned",
            )
        );
    }

    #[test]
    fn test_validation_error_struct() {
        let err = ValidationError::new("priority", "must be 0-4");
        assert_eq!(err.to_string(), "priority: must be 0-4");
    }

    #[test]
    fn committed_state_classification_survives_context_wrapping() {
        let committed = BeadsError::CommittedStateUnwitnessed {
            operation: "sync merge".to_string(),
            source: Box::new(std::io::Error::other("postcommit witness failed")),
        };
        let wrapped = BeadsError::WithContext {
            context: "outer command context".to_string(),
            source: Box::new(committed),
        };

        assert!(wrapped.primary_mutation_committed());
        assert!(!BeadsError::Config("precommit refusal".to_string()).primary_mutation_committed());
    }
}
