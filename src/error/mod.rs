//! Error types and handling for `beads_rust`.
//!
//! This module provides structured errors that match the classic bd
//! behavior for JSON error output compatibility.
//!
//! # Design
//!
//! - Uses `thiserror` for derive-based error types
//! - Supports `anyhow` integration for gradual migration
//! - Provides recovery hints for user-facing errors
//! - Matches bd's exit code conventions
//! - Provides structured JSON output for AI coding agents

mod context;
mod structured;

pub use context::{OptionExt, ResultExt};
pub use structured::{ErrorCode, StructuredError, skip_context, skip_hint};

use crate::model::Status;
use std::path::PathBuf;
use thiserror::Error;

/// The user-facing list of statuses `Status::from_str` accepts.
///
/// Kept honest by `hint_lists_exactly_what_from_str_accepts` in this
/// module's tests, which parses this string back out and checks it
/// against [`crate::model::Status::PARSEABLE`] in both directions —
/// a hardcoded string duplicating a match arm is precisely the thing
/// that goes stale silently.
///
/// The names before the parenthetical are the canonical set, comma
/// separated, so the test can recover them mechanically. Keep that
/// shape if you edit this.
pub const VALID_STATUSES_HINT: &str = "Valid statuses: open, in_progress, blocked, deferred, \
     closed, tombstone, pinned (in_progress also accepts 'inprogress'; tombstone marks a \
     deleted bead and pinned is set by br itself, but both are accepted here — e.g. as \
     filters)";

/// Hint for the specific mistake of `--status all`: `all` is not a
/// status, and the flag actually wanted is `-a`/`--all`.
pub const STATUS_ALL_HINT: &str = "Did you mean -a/--all? 'all' is not a status — \
     use the -a/--all flag for an unfiltered sweep (e.g. `br list -a`), or omit --status.";

/// The opt-in flag that authorizes a write which shrinks a non-empty
/// free-text field on `br update`.
///
/// `--replace` rather than `--force`: `br update --force` already exists and
/// means "claim this issue even though it is blocked". Overloading it would
/// mean an agent bypassing a blocker check silently also authorized data
/// destruction, which is the coupling this guard exists to break. The name
/// also says what it does to the FIELD, which is what the caller needs to
/// think about at the moment they are asked.
pub const REPLACE_FLAG: &str = "--replace";

/// Primary error type for `beads_rust` operations.
///
/// Design: Structured variants for common cases, with `Other` for
/// wrapped anyhow errors during migration.
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
    Database(#[from] rusqlite::Error),

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

    /// Multiple validation errors occurred.
    #[error("Validation errors: {errors:?}")]
    ValidationErrors { errors: Vec<ValidationError> },

    /// A write would shrink a non-empty free-text field, destroying content.
    ///
    /// Refused by default; the caller opts in with `--replace`. See
    /// `crate::validation::text_guard` for the transition rule.
    #[error(
        "Refusing destructive update: {field} on {id} would shrink from {old_chars} to {new_chars} chars"
    )]
    DestructiveFieldShrink {
        /// Issue whose field would be shrunk.
        id: String,
        /// Field name (e.g. `notes`).
        field: String,
        /// CLI flag that sets this field (e.g. `--notes`).
        flag: String,
        /// Size of the stored value, in chars.
        old_chars: usize,
        /// Size of the proposed value, in chars.
        new_chars: usize,
    },

    /// The stored value is not the value bd was asked to store.
    ///
    /// Raised AFTER the write, because that is the earliest moment it can be
    /// observed. bd compares the value it received on the command line with
    /// the value it reads back, so this reports an alteration inside bd — a
    /// caller whose own shell mangled the payload before bd saw it produces a
    /// perfectly consistent write and is not detectable here.
    #[error(
        "Write did not land as sent: {field} on {id} was given {requested_chars} chars but stores {stored_chars}"
    )]
    WriteDidNotLandAsSent {
        /// Issue that was written.
        id: String,
        /// Field name (e.g. `notes`).
        field: String,
        /// Size of the value bd was asked to store, in chars.
        requested_chars: usize,
        /// Size of the value bd actually stored, in chars.
        stored_chars: usize,
    },

    /// Invalid status value.
    #[error("Invalid status: {status}")]
    InvalidStatus { status: String },

    /// Invalid issue type value.
    #[error("Invalid issue type: {issue_type}")]
    InvalidType { issue_type: String },

    /// Priority out of valid range (0-4).
    #[error("Priority must be 0-4, got: {priority}")]
    InvalidPriority { priority: i32 },

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
    Yaml(#[from] serde_yaml::Error),

    // === Wrapped errors (for gradual migration) ===
    /// Error with additional context.
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // === Operational Errors ===
    /// Every requested id was skipped: the state change happened nowhere.
    ///
    /// Carries the per-id [`SkipReason`] rather than a pre-rendered
    /// sentence, because the whole point of `beads1-3c8h4` was that the
    /// specific reason WAS computed and then discarded in favour of a
    /// generic string. The hint and the human warning line are both
    /// rendered from these values, so they cannot contradict each other.
    #[error("Nothing to do: all {} issue(s) skipped", skipped.len())]
    NothingToDo { skipped: Vec<SkippedTarget> },

    /// Some requested ids changed state and some did not.
    ///
    /// A distinct variant from [`Self::NothingToDo`] because "nothing
    /// happened" and "part of it happened" need different recovery: the
    /// second caller has already committed some of the batch and must not
    /// blindly re-run it, and must be told exactly which ids are still
    /// outstanding.
    #[error(
        "Closed {closed} of {} requested issue(s): {} skipped",
        closed + skipped.len(),
        skipped.len()
    )]
    PartiallyClosed {
        /// How many ids actually changed state.
        closed: usize,
        /// The ids that did not, and why.
        skipped: Vec<SkippedTarget>,
    },

    /// Wrapped anyhow error for gradual migration.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// One requested id that did not change state, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTarget {
    /// The id as resolved, not as typed.
    pub id: String,
    /// Why this id was left alone.
    pub reason: SkipReason,
}

impl SkippedTarget {
    /// Record a skip.
    #[must_use]
    pub fn new(id: impl Into<String>, reason: SkipReason) -> Self {
        Self {
            id: id.into(),
            reason,
        }
    }
}

/// Why one requested id was not closed.
///
/// These are genuinely different situations with genuinely different
/// remedies — close the blocker, reopen instead, check the id — and
/// collapsing them into one sentence is what made `br close` tell an
/// agent "already closed or not found" about an open, existing bead
/// (bead `beads1-3c8h4`). Every surface that reports a skip renders it
/// from this enum: [`Self::code`] for machines, [`Self::describe`] for
/// the one-line warning, [`Self::remedy`] for what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Open blockers stand in the way (and `--force` was not given).
    Blocked {
        /// Blocking ids, as `id:status` when the status is known.
        blockers: Vec<String>,
    },
    /// The issue is already in a terminal status.
    ///
    /// Carries the status so a tombstone is never described as "closed":
    /// one is finished work, the other is a deleted bead.
    AlreadyTerminal {
        /// The status found in storage.
        status: Status,
    },
    /// The id resolved but no row came back (deleted underneath us).
    NotFound,
}

impl SkipReason {
    /// Stable machine-readable discriminator.
    ///
    /// This is what a caller keys on instead of string-matching prose:
    /// `blocked`, `already_closed`, `tombstoned`, `not_found`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Blocked { .. } => "blocked",
            Self::AlreadyTerminal { status } => match status {
                Status::Tombstone => "tombstoned",
                _ => "already_closed",
            },
            Self::NotFound => "not_found",
        }
    }

    /// Does the state the caller asked for nevertheless hold?
    ///
    /// This is the question the exit code answers: "is the world in the
    /// state I asked for?", NOT "did bd personally perform a mutation".
    /// An issue that was already closed satisfies `br close` the way an
    /// existing directory satisfies `mkdir -p`, which is what makes the
    /// command safe in the retry loops agents actually write.
    ///
    /// A tombstone does NOT satisfy it: a deleted bead is a different
    /// terminal state from finished work, and a caller whose target was
    /// deleted underneath them should stop rather than proceed.
    #[must_use]
    pub fn end_state_reached(&self) -> bool {
        matches!(
            self,
            Self::AlreadyTerminal {
                status: Status::Closed
            }
        )
    }

    /// The blocking ids, empty for every reason other than blocked.
    #[must_use]
    pub fn blockers(&self) -> &[String] {
        match self {
            Self::Blocked { blockers } => blockers,
            _ => &[],
        }
    }

    /// The one-line reason, as printed next to the id.
    ///
    /// The `Warning: Skipped <id>: <this>` line and the structured
    /// `hint` are both built from this, so the two cannot drift.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Blocked { blockers } if blockers.is_empty() => {
                "blocked by dependencies".to_string()
            }
            Self::Blocked { blockers } => format!("blocked by: {}", blockers.join(", ")),
            Self::AlreadyTerminal { status } => format!("already {}", status.as_str()),
            Self::NotFound => "issue not found".to_string(),
        }
    }

    /// What the caller can do about it, naming only commands that exist
    /// and only ids that can actually be passed to them.
    #[must_use]
    pub fn remedy(&self, ids: &[String]) -> String {
        let targets = ids.join(" ");
        match self {
            Self::Blocked { blockers } => {
                let blocker_ids: Vec<&str> = self
                    .blockers()
                    .iter()
                    .map(|b| blocker_id(b))
                    .collect::<Vec<_>>();
                if blockers.is_empty() || blocker_ids.is_empty() {
                    format!(
                        "Close the blocking dependencies first, or re-run with --force to close {targets} anyway."
                    )
                } else {
                    format!(
                        "Close the blocker(s) first ('br close {}'), or re-run with --force to close {targets} anyway.",
                        blocker_ids.join(" ")
                    )
                }
            }
            Self::AlreadyTerminal { status } => match status {
                Status::Tombstone => format!(
                    "{targets} is a tombstone (a deleted bead), not open work — there is nothing to close."
                ),
                _ => format!(
                    "Nothing changed. Use 'br reopen {targets}' if the work is not actually done."
                ),
            },
            Self::NotFound => {
                format!("Run 'br list' to see available issues, or check the id: {targets}.")
            }
        }
    }
}

/// The bare id part of a blocker rendered as `id:status`.
///
/// A hint that says `br close t-wg7:open` hands the caller a command
/// that cannot run, and advice you cannot follow is how a hint teaches
/// people to stop reading hints.
#[must_use]
pub fn blocker_id(blocker: &str) -> &str {
    blocker.split(':').next().unwrap_or(blocker)
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
                | Self::DestructiveFieldShrink { .. }
                | Self::InvalidStatus { .. }
                | Self::InvalidType { .. }
                | Self::InvalidPriority { .. }
                | Self::PrefixMismatch { .. }
                | Self::AmbiguousId { .. }
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

    /// Human-friendly suggestion for fixing this error.
    #[must_use]
    pub const fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::NotInitialized => Some("Run: br init"),
            Self::DatabaseNotFound { .. } => Some("Check path or run: br init"),
            Self::AmbiguousId { .. } => Some("Provide more characters of the ID"),
            Self::HasDependents { .. } => Some("Use --force or --cascade to delete anyway"),
            Self::ImportCollision { .. } => Some("Use --force to overwrite or resolve manually"),
            Self::DependencyCycle { .. } => Some("Remove one dependency to break the cycle"),
            Self::SelfDependency { .. } => Some("An issue cannot depend on itself"),
            Self::AlreadyInitialized { .. } => Some("Use --force to reinitialize"),
            Self::InvalidPriority { .. } => {
                Some("Use a priority between 0 (critical) and 4 (backlog)")
            }
            Self::InvalidStatus { .. } => Some(VALID_STATUSES_HINT),
            Self::InvalidType { .. } => Some("Valid types: task, bug, feature, epic, chore"),
            _ => None,
        }
    }

    /// Get the exit code for this error.
    ///
    /// Legacy bd typically uses exit code 1 for most errors.
    /// `NothingToDo` uses exit code 3 (issue errors category).
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NothingToDo { .. } | Self::PartiallyClosed { .. } => 3,
            _ => 1,
        }
    }

    /// Create a validation error for a specific field.
    #[must_use]
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// Create from multiple validation errors.
    #[must_use]
    pub fn from_validation_errors(errors: Vec<ValidationError>) -> Self {
        if errors.len() == 1 {
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
    fn test_user_recoverable() {
        let recoverable = BeadsError::NotInitialized;
        assert!(recoverable.is_user_recoverable());

        let not_recoverable = BeadsError::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(1),
            None,
        ));
        assert!(!not_recoverable.is_user_recoverable());
    }

    /// The INVALID_STATUS hint and `Status::from_str` must describe the
    /// same set, in both directions. A hint that under-reports what the
    /// parser accepts is a real defect: it tells an agent that
    /// `--status tombstone` is invalid when it works fine (bead
    /// `beads1-17zqr`).
    ///
    /// The `listed_in_hint` match below is deliberately exhaustive with
    /// no wildcard arm: adding a `Status` variant will fail to compile
    /// here, forcing whoever adds it to decide whether it is
    /// user-selectable and to say so in the hint.
    #[test]
    fn hint_lists_exactly_what_from_str_accepts() {
        use crate::model::Status;
        use std::str::FromStr;

        const fn listed_in_hint(status: &Status) -> bool {
            match status {
                Status::Open
                | Status::InProgress
                | Status::Blocked
                | Status::Deferred
                | Status::Closed
                | Status::Tombstone
                | Status::Pinned => true,
                // Unreachable through `from_str` (which rejects unknown
                // names); `Custom` only arrives via serde
                // deserialization of foreign JSONL, so it is not a
                // value a user can pass to `--status`.
                Status::Custom(_) => false,
            }
        }

        let every_variant = [
            Status::Open,
            Status::InProgress,
            Status::Blocked,
            Status::Deferred,
            Status::Closed,
            Status::Tombstone,
            Status::Pinned,
            Status::Custom("something-else".to_string()),
        ];

        for status in &every_variant {
            let name = status.as_str();
            let expected = listed_in_hint(status);
            assert_eq!(
                Status::from_str(name).is_ok(),
                expected,
                "from_str acceptance of '{name}' disagrees with whether it is user-selectable"
            );
            assert_eq!(
                Status::PARSEABLE.contains(&name),
                expected,
                "Status::PARSEABLE disagrees about '{name}'"
            );
        }

        // Forward direction: every name the parser accepts is listed.
        for name in Status::PARSEABLE {
            assert!(
                VALID_STATUSES_HINT.contains(name),
                "VALID_STATUSES_HINT omits '{name}', which Status::from_str accepts"
            );
        }
        for (alias, canonical) in Status::ALIASES {
            assert!(
                Status::from_str(alias).is_ok(),
                "alias '{alias}' should parse"
            );
            assert_eq!(Status::from_str(alias).unwrap().as_str(), canonical);
            assert!(
                VALID_STATUSES_HINT.contains(alias),
                "VALID_STATUSES_HINT omits the accepted alias '{alias}'"
            );
        }

        // Reverse direction: every name the hint advertises parses, and
        // it advertises exactly the canonical set (no extras, none
        // missing). The hint's canonical list is the comma-separated
        // run between "Valid statuses: " and the parenthetical.
        let listed = VALID_STATUSES_HINT
            .strip_prefix("Valid statuses: ")
            .expect("hint should start with 'Valid statuses: '")
            .split(" (")
            .next()
            .expect("split always yields one element");
        let advertised: Vec<&str> = listed.split(", ").map(str::trim).collect();
        for name in &advertised {
            assert!(
                Status::from_str(name).is_ok(),
                "hint advertises '{name}', which Status::from_str rejects"
            );
        }
        assert_eq!(
            advertised,
            Status::PARSEABLE.to_vec(),
            "hint's status list must match Status::PARSEABLE exactly (same order)"
        );
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
    }

    #[test]
    fn test_validation_error_struct() {
        let err = ValidationError::new("priority", "must be 0-4");
        assert_eq!(err.to_string(), "priority: must be 0-4");
    }

    // =========================================================================
    // Skip reasons (bead beads1-3c8h4)
    // =========================================================================

    #[test]
    fn skip_reason_codes_are_stable_and_distinct() {
        // These strings are the machine contract: a caller keys on them
        // instead of matching prose, so changing one is a breaking change.
        assert_eq!(
            SkipReason::Blocked {
                blockers: vec!["t-b".to_string()]
            }
            .code(),
            "blocked"
        );
        assert_eq!(
            SkipReason::AlreadyTerminal {
                status: Status::Closed
            }
            .code(),
            "already_closed"
        );
        assert_eq!(
            SkipReason::AlreadyTerminal {
                status: Status::Tombstone
            }
            .code(),
            "tombstoned"
        );
        assert_eq!(SkipReason::NotFound.code(), "not_found");
    }

    #[test]
    fn only_an_already_closed_issue_counts_as_the_requested_end_state() {
        assert!(
            SkipReason::AlreadyTerminal {
                status: Status::Closed
            }
            .end_state_reached(),
            "already closed satisfies close (mkdir -p semantics)"
        );
        // A tombstone is a DELETED bead, not finished work. Treating it as
        // satisfied would let an agent conclude it closed something that no
        // longer exists.
        assert!(
            !SkipReason::AlreadyTerminal {
                status: Status::Tombstone
            }
            .end_state_reached(),
            "a tombstone is not the end state the caller asked for"
        );
        assert!(
            !SkipReason::Blocked {
                blockers: vec!["t-b".to_string()]
            }
            .end_state_reached()
        );
        assert!(!SkipReason::NotFound.end_state_reached());
    }

    #[test]
    fn describe_for_blocked_names_the_blockers_it_was_given() {
        // Found by mutation M10: replacing this arm with the generic
        // "blocked by dependencies" passed every other test, because the
        // blocker ids reach the hint through `remedy` and the JSON through
        // `blockers()`. Nothing asserted that the line a HUMAN reads
        // ("Warning: Skipped X: blocked by: ...") still names them — and
        // that line is the one thing the old code got right.
        let reason = SkipReason::Blocked {
            blockers: vec!["t-b:open".to_string(), "t-c:in_progress".to_string()],
        };
        let described = reason.describe();
        assert!(described.contains("t-b:open"), "{described}");
        assert!(described.contains("t-c:in_progress"), "{described}");
        // The generic phrasing is reserved for the case where the blockers
        // genuinely could not be determined.
        assert_ne!(described, "blocked by dependencies");
        assert_eq!(
            SkipReason::Blocked { blockers: vec![] }.describe(),
            "blocked by dependencies"
        );
    }

    #[test]
    fn describe_never_calls_a_tombstone_closed() {
        let described = SkipReason::AlreadyTerminal {
            status: Status::Tombstone,
        }
        .describe();
        assert!(described.contains("tombstone"), "{described}");
        assert_ne!(described, "already closed");
    }

    #[test]
    fn remedy_for_blocked_suggests_a_command_that_can_actually_run() {
        let reason = SkipReason::Blocked {
            blockers: vec!["t-b:open".to_string(), "t-c:in_progress".to_string()],
        };
        let remedy = reason.remedy(&["t-a".to_string()]);
        // `br close t-b:open` is not a runnable command.
        assert!(remedy.contains("br close t-b t-c"), "{remedy}");
        assert!(!remedy.contains(":open"), "{remedy}");
        assert!(remedy.contains("--force"), "{remedy}");
    }

    #[test]
    fn remedy_for_blocked_without_known_blockers_still_says_something_useful() {
        let reason = SkipReason::Blocked { blockers: vec![] };
        let remedy = reason.remedy(&["t-a".to_string()]);
        assert!(remedy.contains("blocking dependencies"), "{remedy}");
        assert!(remedy.contains("--force"), "{remedy}");
        // No empty quotes where a command should be.
        assert!(!remedy.contains("''"), "{remedy}");
    }

    #[test]
    fn blocker_id_strips_the_status_suffix() {
        assert_eq!(blocker_id("t-b:open"), "t-b");
        assert_eq!(blocker_id("t-b"), "t-b");
        assert_eq!(blocker_id(""), "");
    }

    #[test]
    fn skipped_batches_never_exit_zero_at_the_error_layer() {
        // Whatever the shape, an error that reaches this layer must carry a
        // non-zero exit code: exiting 0 while printing an "error" object is
        // the defect in bead beads1-3c8h4.
        let nothing = BeadsError::NothingToDo {
            skipped: vec![SkippedTarget::new("t-a", SkipReason::NotFound)],
        };
        let partial = BeadsError::PartiallyClosed {
            closed: 1,
            skipped: vec![SkippedTarget::new(
                "t-a",
                SkipReason::Blocked {
                    blockers: vec!["t-b:open".to_string()],
                },
            )],
        };
        assert_eq!(nothing.exit_code(), 3);
        assert_eq!(partial.exit_code(), 3);
        assert_ne!(nothing.exit_code(), 0);
        assert_ne!(partial.exit_code(), 0);
        // The messages must not be interchangeable: "nothing happened" and
        // "part of it happened" need different recovery.
        assert!(nothing.to_string().contains("Nothing to do"));
        assert!(partial.to_string().contains("Closed 1 of 2"));
    }
}
