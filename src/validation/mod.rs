//! Validation helpers for `beads_rust`.
//!
//! These routines enforce classic bd data constraints and return
//! structured validation errors without mutating storage.
//!
//! # Sync Safety Guarantees
//!
//! The sync subsystem enforces these invariants by design:
//! - **No git operations**: br sync NEVER executes git commands
//! - **Path confinement**: All I/O stays within `.beads/` (unless explicitly opted-in)
//! - **No .git access**: Sync code paths never read from or write to `.git/`
//!
//! See `SyncSafetyValidator` for runtime guards.

use crate::error::{BeadsError, ValidationError};
use crate::model::{Comment, Dependency, DependencyType, Issue, Priority, Status};
use crate::util::id::MAX_ID_LENGTH;
use std::fs;
use std::path::Path;

const TITLE_MAX_CHARS: usize = 500;
const ACTOR_MAX_CHARS: usize = 200;
const CUSTOM_VARIANT_MAX_CHARS: usize = 50;
pub(crate) const ISSUE_LABEL_MAX_COUNT: usize = 64;

/// Validates issue fields and invariants.
pub struct IssueValidator;

impl IssueValidator {
    /// Validate an issue and return all validation errors found.
    ///
    /// # Errors
    ///
    /// Returns a `Vec<ValidationError>` if any validation rules are violated.
    pub fn validate(issue: &Issue) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        // ID: Required, max length, prefix-hash format.
        if issue.id.trim().is_empty() {
            errors.push(ValidationError::new("id", "cannot be empty"));
        }
        if issue.id.len() > MAX_ID_LENGTH {
            errors.push(ValidationError::new(
                "id",
                format!("exceeds {MAX_ID_LENGTH} characters"),
            ));
        }
        if !issue.id.is_empty() && !is_valid_id_format(&issue.id) {
            errors.push(ValidationError::new(
                "id",
                "invalid format (expected prefix-hash)",
            ));
        }

        validate_issue_text_fields(issue, &mut errors);

        // Priority: 0-4 range.
        if issue.priority.0 < Priority::CRITICAL.0 || issue.priority.0 > Priority::BACKLOG.0 {
            errors.push(ValidationError::new("priority", "must be 0-4"));
        }

        // Timestamps: created_at <= updated_at.
        if issue.updated_at < issue.created_at {
            errors.push(ValidationError::new(
                "updated_at",
                "cannot be before created_at",
            ));
        }

        // Estimated minutes: Optional, must be non-negative and reasonable.
        if let Some(minutes) = issue.estimated_minutes {
            if minutes < 0 {
                errors.push(ValidationError::new(
                    "estimated_minutes",
                    "cannot be negative",
                ));
            } else if minutes > 525_960 {
                // ~1 year in minutes
                errors.push(ValidationError::new(
                    "estimated_minutes",
                    "exceeds maximum (525960 minutes / ~1 year)",
                ));
            }
        }

        if issue.status == Status::Closed && issue.closed_at.is_none() {
            errors.push(ValidationError::new(
                "closed_at",
                "closed issues must set closed_at",
            ));
        }

        if !matches!(issue.status, Status::Closed | Status::Tombstone) && issue.closed_at.is_some()
        {
            errors.push(ValidationError::new(
                "closed_at",
                "only closed or tombstone issues may set closed_at",
            ));
        }

        // Closed timestamps: closed_at must not precede created_at.
        if let Some(closed_at) = issue.closed_at
            && closed_at < issue.created_at
        {
            errors.push(ValidationError::new(
                "closed_at",
                "cannot be before created_at",
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn validate_issue_text_fields(issue: &Issue, errors: &mut Vec<ValidationError>) {
    // Title: Required, max 500 chars.
    if issue.title.trim().is_empty() {
        errors.push(ValidationError::new("title", "cannot be empty"));
    }
    if issue.title.chars().count() > TITLE_MAX_CHARS {
        errors.push(ValidationError::new("title", "exceeds 500 characters"));
    }
    reject_nul("title", &issue.title, errors);

    // Long-text fields (description, design, acceptance_criteria, notes) are
    // unbounded by design — these capture full specs, RFC text, agent
    // session transcripts, etc. A prior 100KB cap rejected legitimate
    // pre-existing records on JSONL rebuild and blocked workspace recovery
    // (frankensqlite .beads had nine records up to 554KB that were valid
    // bead bodies, not corruption). We still reject NUL bytes for SQLite
    // compatibility.
    if let Some(s) = issue.description.as_deref() {
        reject_nul("description", s, errors);
    }
    if let Some(s) = issue.design.as_deref() {
        reject_nul("design", s, errors);
    }
    if let Some(s) = issue.acceptance_criteria.as_deref() {
        reject_nul("acceptance_criteria", s, errors);
    }
    if let Some(s) = issue.notes.as_deref() {
        reject_nul("notes", s, errors);
    }
    reject_nul("status", issue.status.as_str(), errors);
    validate_custom_status(&issue.status, errors);
    reject_nul("issue_type", issue.issue_type.as_str(), errors);
    validate_custom_issue_type(&issue.issue_type, errors);
    reject_bounded_chars_opt(
        "assignee",
        issue.assignee.as_deref(),
        ACTOR_MAX_CHARS,
        errors,
    );
    reject_bounded_chars_opt("owner", issue.owner.as_deref(), ACTOR_MAX_CHARS, errors);
    reject_bounded_chars_opt(
        "created_by",
        issue.created_by.as_deref(),
        ACTOR_MAX_CHARS,
        errors,
    );
    validate_external_ref(issue.external_ref.as_deref(), errors);
    reject_bounded_chars_opt(
        "source_system",
        issue.source_system.as_deref(),
        ACTOR_MAX_CHARS,
        errors,
    );
    validate_issue_labels(issue, errors);
}

fn validate_external_ref(external_ref: Option<&str>, errors: &mut Vec<ValidationError>) {
    if let Some(external_ref) = external_ref {
        reject_nul("external_ref", external_ref, errors);
        if external_ref.len() > 200 {
            errors.push(ValidationError::new(
                "external_ref",
                "exceeds 200 characters",
            ));
        }
        if external_ref.chars().any(char::is_whitespace) {
            errors.push(ValidationError::new(
                "external_ref",
                "cannot contain whitespace",
            ));
        }
    }
}

fn reject_nul(field: &str, value: &str, errors: &mut Vec<ValidationError>) {
    if value.contains('\0') {
        errors.push(ValidationError::new(field, "cannot contain NUL bytes"));
    }
}

fn reject_bounded_chars_opt(
    field: &str,
    value: Option<&str>,
    max_chars: usize,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(value) = value {
        reject_nul(field, value, errors);
        if value.chars().count() > max_chars {
            errors.push(ValidationError::new(
                field,
                format!("exceeds {max_chars} characters"),
            ));
        }
    }
}

fn validate_custom_status(status: &Status, errors: &mut Vec<ValidationError>) {
    if let Status::Custom(value) = status
        && value.chars().count() > CUSTOM_VARIANT_MAX_CHARS
    {
        errors.push(ValidationError::new(
            "status",
            "custom status exceeds 50 characters",
        ));
    }
}

fn validate_custom_issue_type(
    issue_type: &crate::model::IssueType,
    errors: &mut Vec<ValidationError>,
) {
    if let crate::model::IssueType::Custom(value) = issue_type
        && value.chars().count() > CUSTOM_VARIANT_MAX_CHARS
    {
        errors.push(ValidationError::new(
            "issue_type",
            "custom issue type exceeds 50 characters",
        ));
    }
}

fn validate_issue_labels(issue: &Issue, errors: &mut Vec<ValidationError>) {
    if issue.labels.len() > ISSUE_LABEL_MAX_COUNT {
        errors.push(ValidationError::new(
            "labels",
            format!("exceeds {ISSUE_LABEL_MAX_COUNT} labels"),
        ));
    }

    for (idx, label) in issue.labels.iter().enumerate() {
        if let Err(err) = LabelValidator::validate(label) {
            errors.push(ValidationError::new(
                "labels",
                format!("label at index {idx}: {}", err.message),
            ));
        }
    }
}

/// Storage-facing dependency validation helpers.
pub trait DependencyStore {
    /// Return true if the issue exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage lookup fails.
    fn issue_exists(&self, id: &str) -> Result<bool, BeadsError>;
    /// Return true if the dependency edge already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage lookup fails.
    fn dependency_exists(&self, issue_id: &str, depends_on_id: &str) -> Result<bool, BeadsError>;
    /// Return true if adding the dependency would create a cycle.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage lookup fails.
    fn would_create_cycle(&self, issue_id: &str, depends_on_id: &str) -> Result<bool, BeadsError>;

    /// Return true if adding a stored `parent-child` row would create a cycle.
    ///
    /// Stored parent-child rows are child -> parent, while the blocking graph
    /// treats them as parent -> child. Stores that model `would_create_cycle`
    /// as a blocking-graph edge can use this default.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage lookup fails.
    fn would_create_parent_child_cycle(
        &self,
        child_id: &str,
        parent_id: &str,
    ) -> Result<bool, BeadsError> {
        self.would_create_cycle(parent_id, child_id)
    }

    /// Return true if adding the typed dependency would create a cycle.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage lookup fails.
    fn would_create_dependency_cycle(
        &self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &DependencyType,
    ) -> Result<bool, BeadsError> {
        if matches!(dep_type, DependencyType::ParentChild) {
            self.would_create_parent_child_cycle(issue_id, depends_on_id)
        } else {
            self.would_create_cycle(issue_id, depends_on_id)
        }
    }
}

/// Validates dependency invariants, optionally consulting storage.
pub struct DependencyValidator;

impl DependencyValidator {
    /// Validate dependency rules, returning a `BeadsError` on storage failures.
    ///
    /// # Errors
    ///
    /// Returns a `BeadsError` if storage lookups fail or validation fails.
    pub fn validate(dep: &Dependency, store: &impl DependencyStore) -> Result<(), BeadsError> {
        let mut errors = Vec::new();

        if dep.issue_id == dep.depends_on_id {
            errors.push(ValidationError::new(
                "depends_on_id",
                "issue cannot depend on itself",
            ));
        }

        if !store.issue_exists(&dep.issue_id)? {
            errors.push(ValidationError::new("issue_id", "issue not found"));
        }

        if !store.issue_exists(&dep.depends_on_id)? {
            errors.push(ValidationError::new(
                "depends_on_id",
                "dependency target not found",
            ));
        }

        if dep.dep_type.is_blocking()
            && store.would_create_dependency_cycle(
                &dep.issue_id,
                &dep.depends_on_id,
                &dep.dep_type,
            )?
        {
            errors.push(ValidationError::new(
                "depends_on_id",
                "would create dependency cycle",
            ));
        }

        if store.dependency_exists(&dep.issue_id, &dep.depends_on_id)? {
            errors.push(ValidationError::new(
                "depends_on_id",
                "dependency already exists",
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(BeadsError::from_validation_errors(errors))
        }
    }
}

/// Validates a single label value.
pub struct LabelValidator;

impl LabelValidator {
    /// Validate a label for length and allowed characters.
    ///
    /// # Errors
    ///
    /// Returns a `ValidationError` if the label is invalid.
    pub fn validate(label: &str) -> Result<(), ValidationError> {
        if label.is_empty() {
            return Err(ValidationError::new("label", "cannot be empty"));
        }

        if label.len() > 50 {
            return Err(ValidationError::new("label", "exceeds 50 characters"));
        }

        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
        {
            return Err(ValidationError::new(
                "label",
                "invalid characters (only alphanumeric, hyphen, underscore, colon allowed)",
            ));
        }

        Ok(())
    }
}

/// Validates comment fields.
pub struct CommentValidator;

impl CommentValidator {
    /// Validate a comment and return all validation errors found.
    ///
    /// # Errors
    ///
    /// Returns a `Vec<ValidationError>` if any validation rules are violated.
    pub fn validate(comment: &Comment) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        if comment.id <= 0 {
            errors.push(ValidationError::new("id", "must be positive"));
        }

        if comment.issue_id.trim().is_empty() {
            errors.push(ValidationError::new("issue_id", "cannot be empty"));
        }

        if comment.body.trim().is_empty() {
            errors.push(ValidationError::new("content", "cannot be empty"));
        }

        // Comment bodies are unbounded — same reasoning as long-text issue
        // fields above. Reject only NUL bytes for SQLite compatibility.
        reject_nul("content", &comment.body, &mut errors);

        if comment.author.trim().is_empty() {
            errors.push(ValidationError::new("author", "cannot be empty"));
        }

        if comment.author.len() > 200 {
            errors.push(ValidationError::new("author", "exceeds 200 characters"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[must_use]
pub fn is_valid_id_format(id: &str) -> bool {
    crate::util::id::is_valid_id_format(id)
}

// =============================================================================
// SYNC SAFETY VALIDATION
// =============================================================================

/// Validates sync operations adhere to safety invariants.
///
/// # Safety Guarantees (Non-Goals - What br sync NEVER does)
///
/// 1. **No git commands**: br sync never executes `git` subprocess commands
/// 2. **No git library calls**: No gitoxide, libgit2, or similar
/// 3. **No .git access**: Never reads from or writes to `.git/` directory
/// 4. **No auto-commit**: All git operations are user-initiated
/// 5. **No hook execution**: No git hooks are installed or triggered
///
/// These are defended by a fail-closed source-boundary scan, a direct
/// normal/target-runtime manifest guard, a separately reviewed
/// `cargo tree -e normal`, and the runtime PATH/.git snapshot matrix.
pub struct SyncSafetyValidator;

impl SyncSafetyValidator {
    const FORBIDDEN_AUTHORITY_PATTERNS: [(&'static str, &'static str); 17] = [
        (
            "std::process::Command",
            "direct subprocess command construction",
        ),
        ("process::Command", "subprocess command construction"),
        ("std::process::{", "aliased subprocess command import"),
        ("process::{", "aliased subprocess command import"),
        ("usestd::processas", "aliased subprocess module import"),
        ("usestd::{processas", "aliased subprocess module import"),
        ("Command::new", "subprocess command construction"),
        (
            "crate::cli::commands::",
            "delegation to a process-capable CLI adapter namespace",
        ),
        (
            "include!(",
            "source inclusion that can evade the inspected boundary",
        ),
        (
            "include_bytes!(",
            "byte inclusion that can hide authority-bearing source",
        ),
        (
            "#[path",
            "out-of-bound module path inclusion that can evade inspection",
        ),
        ("run_git", "Git subprocess wrapper"),
        ("spawn_git", "Git subprocess wrapper"),
        ("git_capture", "Git subprocess wrapper"),
        ("git2::", "Git library authority"),
        ("gix::", "Git library authority"),
        ("gitoxide", "Git library authority"),
    ];

    /// Validates that a path does not target git internals.
    ///
    /// Returns an error if the path contains `.git` components.
    ///
    /// # Errors
    ///
    /// Returns `ValidationError` if path contains `.git`.
    pub fn validate_no_git_path(path: &Path) -> Result<(), ValidationError> {
        // Check each component of the path for .git
        for component in path.components() {
            if let std::path::Component::Normal(name) = component
                && name == ".git"
            {
                return Err(ValidationError::new(
                    "path",
                    "sync operations cannot access .git directory (safety invariant)",
                ));
            }
        }

        // Also check the string representation for hidden .git references
        let path_str = path.to_string_lossy();
        if path_str.contains("/.git/")
            || path_str.contains("\\.git\\")
            || path_str.ends_with("/.git")
            || path_str.ends_with("\\.git")
        {
            return Err(ValidationError::new(
                "path",
                "sync operations cannot access .git directory (safety invariant)",
            ));
        }

        Ok(())
    }

    /// Validates that a path is within the allowed beads directory.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to validate
    /// * `beads_dir` - The .beads directory that contains allowed paths
    /// * `allow_external` - Whether external paths are permitted (opt-in)
    ///
    /// # Errors
    ///
    /// Returns `ValidationError` if path escapes the allowlist.
    pub fn validate_path_containment(
        path: &Path,
        beads_dir: &Path,
        allow_external: bool,
    ) -> Result<(), ValidationError> {
        // First, ensure no .git access
        Self::validate_no_git_path(path)?;

        // If external paths are allowed, skip containment check
        if allow_external {
            return Ok(());
        }

        // Canonicalize if possible, otherwise use the path as-is
        let canonical_path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canonical_beads =
            dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());

        // Check if path starts with beads_dir
        if !canonical_path.starts_with(&canonical_beads) {
            return Err(ValidationError::new(
                "path",
                format!(
                    "path '{}' is outside allowed directory '{}' \
                     (use --allow-external-jsonl to override)",
                    path.display(),
                    beads_dir.display()
                ),
            ));
        }

        Ok(())
    }

    /// Validate that every Rust source in the sync authority boundary is free
    /// of subprocess, Git-library, and VCS-adapter authority.
    ///
    /// The boundary deliberately includes both the reusable sync engine
    /// (`src/sync/**/*.rs`) and the CLI sync adapter
    /// (`src/cli/commands/sync.rs`). The walk is fail-closed: a missing path,
    /// unreadable or special entry, symlink, non-UTF-8 source, or forbidden
    /// construct is an error rather than a skipped check. Whitespace is
    /// normalized before matching so split-token formatting cannot evade the
    /// guard. The scan is intentionally conservative over comments and string
    /// literals: a false positive requires moving or rewording documentation,
    /// whereas ignoring source regions would create an authority-evasion
    /// surface.
    ///
    /// # Errors
    ///
    /// Returns `ValidationError` when the source boundary cannot be inspected
    /// completely or when authority-bearing code is found.
    pub fn validate_no_git_authority_in_sync_sources(
        repo_root: &Path,
    ) -> Result<(), ValidationError> {
        let sync_dir = repo_root.join("src/sync");
        let cli_sync = repo_root.join("src/cli/commands/sync.rs");

        Self::validate_sync_source_tree(repo_root, &sync_dir)?;
        Self::validate_sync_source_file(repo_root, &cli_sync)
    }

    fn validate_sync_source_tree(
        repo_root: &Path,
        directory: &Path,
    ) -> Result<(), ValidationError> {
        let metadata = fs::symlink_metadata(directory).map_err(|error| {
            Self::sync_source_error(
                repo_root,
                directory,
                format!("cannot inspect required directory: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(Self::sync_source_error(
                repo_root,
                directory,
                "required directory is a symlink",
            ));
        }
        if !metadata.is_dir() {
            return Err(Self::sync_source_error(
                repo_root,
                directory,
                "required sync source directory is not a directory",
            ));
        }

        let mut entries = fs::read_dir(directory)
            .map_err(|error| {
                Self::sync_source_error(
                    repo_root,
                    directory,
                    format!("cannot read required directory: {error}"),
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                Self::sync_source_error(
                    repo_root,
                    directory,
                    format!("cannot enumerate required directory: {error}"),
                )
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                Self::sync_source_error(
                    repo_root,
                    &path,
                    format!("cannot inspect source entry: {error}"),
                )
            })?;
            if file_type.is_symlink() {
                return Err(Self::sync_source_error(
                    repo_root,
                    &path,
                    "sync source entry is a symlink",
                ));
            }
            if file_type.is_dir() {
                Self::validate_sync_source_tree(repo_root, &path)?;
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                Self::validate_sync_source_file(repo_root, &path)?;
            } else if !file_type.is_file() {
                return Err(Self::sync_source_error(
                    repo_root,
                    &path,
                    "sync source tree contains an unsupported special entry",
                ));
            }
        }
        Ok(())
    }

    fn validate_sync_source_file(repo_root: &Path, path: &Path) -> Result<(), ValidationError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            Self::sync_source_error(
                repo_root,
                path,
                format!("cannot inspect required source: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(Self::sync_source_error(
                repo_root,
                path,
                "required sync source is a symlink",
            ));
        }
        if !metadata.is_file() {
            return Err(Self::sync_source_error(
                repo_root,
                path,
                "required sync source is not a regular file",
            ));
        }

        let source = fs::read_to_string(path).map_err(|error| {
            Self::sync_source_error(
                repo_root,
                path,
                format!("cannot read required UTF-8 source: {error}"),
            )
        })?;
        Self::validate_sync_source_text(
            path.strip_prefix(repo_root).unwrap_or(path),
            source.as_str(),
        )
    }

    fn validate_sync_source_text(
        relative_path: &Path,
        source: &str,
    ) -> Result<(), ValidationError> {
        let normalized = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        for (pattern, authority) in Self::FORBIDDEN_AUTHORITY_PATTERNS {
            if normalized.contains(pattern) {
                return Err(ValidationError::new(
                    "sync_source",
                    format!(
                        "{} contains forbidden {authority} marker {pattern:?}",
                        relative_path.display()
                    ),
                ));
            }
        }
        if relative_path == Path::new("src/cli/commands/sync.rs")
            && Self::references_process_capable_sibling(&normalized)
        {
            return Err(ValidationError::new(
                "sync_source",
                format!(
                    "{} contains a forbidden process-capable CLI sibling reference",
                    relative_path.display()
                ),
            ));
        }
        Ok(())
    }

    fn references_process_capable_sibling(normalized: &str) -> bool {
        const ADAPTERS: [&str; 8] = [
            "changelog",
            "config",
            "doctor",
            "orphans",
            "stats",
            "upgrade",
            "vcs",
            "version",
        ];

        ADAPTERS.iter().any(|adapter| {
            normalized.contains(&format!("super::{adapter}::"))
                || normalized.contains(&format!("super::{adapter};"))
                || normalized.contains(&format!("super::{adapter}as"))
                || grouped_super_import_contains(normalized, adapter)
        })
    }

    fn sync_source_error(
        repo_root: &Path,
        path: &Path,
        reason: impl std::fmt::Display,
    ) -> ValidationError {
        ValidationError::new(
            "sync_source",
            format!(
                "{}: {reason}",
                path.strip_prefix(repo_root).unwrap_or(path).display()
            ),
        )
    }
}

fn grouped_super_import_contains(normalized: &str, adapter: &str) -> bool {
    let mut remaining = normalized;
    while let Some(start) = remaining.find("usesuper::{") {
        remaining = &remaining[start + "usesuper::{".len()..];
        let Some(end) = remaining.find('}') else {
            return true;
        };
        if remaining[..end].split(',').any(|import| {
            import == adapter
                || import.starts_with(&format!("{adapter}as"))
                || import.starts_with(&format!("{adapter}::"))
        }) {
            return true;
        }
        remaining = &remaining[end + 1..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DependencyType, IssueType, Status};
    use chrono::{TimeZone, Utc};

    fn base_issue() -> Issue {
        Issue {
            id: "bd-abc123".to_string(),
            content_hash: None,
            title: "Test issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
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
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }

    #[test]
    fn issue_validation_rejects_empty_title() {
        let mut issue = base_issue();
        issue.title = " ".to_string();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "title"));
    }

    #[test]
    fn issue_validation_counts_title_limit_in_chars_not_utf8_bytes() {
        let mut issue = base_issue();
        issue.title = "\u{1f980}".repeat(500);
        assert!(IssueValidator::validate(&issue).is_ok());

        issue.title = "\u{1f980}".repeat(501);
        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "title"));
    }

    #[test]
    fn issue_validation_rejects_nul_in_content_hash_fields() {
        let mut issue = base_issue();
        issue.title = "nul\0title".to_string();
        issue.description = Some("nul\0description".to_string());
        issue.design = Some("nul\0design".to_string());
        issue.acceptance_criteria = Some("nul\0acceptance".to_string());
        issue.notes = Some("nul\0notes".to_string());
        issue.status = Status::Custom("nul\0status".to_string());
        issue.issue_type = IssueType::Custom("nul\0type".to_string());
        issue.assignee = Some("nul\0assignee".to_string());
        issue.owner = Some("nul\0owner".to_string());
        issue.created_by = Some("nul\0creator".to_string());
        issue.external_ref = Some("nul\0external".to_string());
        issue.source_system = Some("nul\0source".to_string());

        let errors = IssueValidator::validate(&issue).unwrap_err();
        let fields: Vec<_> = errors.iter().map(|err| err.field.as_str()).collect();
        for field in [
            "title",
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "status",
            "issue_type",
            "assignee",
            "owner",
            "created_by",
            "external_ref",
            "source_system",
        ] {
            assert!(fields.contains(&field), "missing NUL rejection for {field}");
        }
    }

    #[test]
    fn issue_validation_rejects_invalid_id() {
        let mut issue = base_issue();
        issue.id = "invalid".to_string();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "id"));
    }

    #[test]
    fn issue_validation_rejects_priority_out_of_range() {
        let mut issue = base_issue();
        issue.priority = Priority(9);

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "priority"));
    }

    #[test]
    fn issue_validation_accepts_arbitrarily_large_description() {
        // Long-text fields (description / design / acceptance_criteria /
        // notes) are intentionally unbounded — spec write-ups, RFC text,
        // and agent session transcripts routinely exceed any small cap.
        let mut issue = base_issue();
        issue.description = Some("x".repeat(600_000));

        IssueValidator::validate(&issue).expect("long descriptions must validate cleanly");
    }

    #[test]
    fn issue_validation_rejects_closed_without_closed_at() {
        let mut issue = base_issue();
        issue.status = Status::Closed;

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "closed_at"));
    }

    #[test]
    fn issue_validation_rejects_non_terminal_closed_at() {
        let mut issue = base_issue();
        issue.closed_at = Some(issue.updated_at);

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "closed_at"));
    }

    #[test]
    fn issue_validation_allows_tombstone_without_closed_at() {
        let mut issue = base_issue();
        issue.status = Status::Tombstone;

        assert!(IssueValidator::validate(&issue).is_ok());
    }

    #[test]
    fn label_validation_rejects_invalid_characters() {
        let err = LabelValidator::validate("bad label").unwrap_err();
        assert_eq!(err.field, "label");

        let err = LabelValidator::validate("has/slash").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn label_validation_rejects_empty() {
        let err = LabelValidator::validate("").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn label_validation_allows_namespaced_labels() {
        assert!(LabelValidator::validate("team:backend").is_ok());
    }

    #[test]
    fn label_validation_rejects_path_style_labels() {
        let err = LabelValidator::validate("sys/stat").unwrap_err();
        assert_eq!(err.field, "label");
    }

    #[test]
    fn comment_validation_rejects_empty_body() {
        let comment = Comment {
            id: 1,
            issue_id: "bd-abc123".to_string(),
            author: "tester".to_string(),
            body: " ".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        };

        let errors = CommentValidator::validate(&comment).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "content"));
    }

    #[allow(clippy::struct_excessive_bools)]
    struct FakeStore {
        issue_exists: bool,
        depends_on_exists: bool,
        dependency_exists: bool,
        would_cycle: bool,
    }

    impl DependencyStore for FakeStore {
        fn issue_exists(&self, id: &str) -> Result<bool, BeadsError> {
            Ok(match id {
                "issue" => self.issue_exists,
                _ => self.depends_on_exists,
            })
        }

        fn dependency_exists(
            &self,
            _issue_id: &str,
            _depends_on_id: &str,
        ) -> Result<bool, BeadsError> {
            Ok(self.dependency_exists)
        }

        fn would_create_cycle(
            &self,
            _issue_id: &str,
            _depends_on_id: &str,
        ) -> Result<bool, BeadsError> {
            Ok(self.would_cycle)
        }
    }

    fn base_dependency() -> Dependency {
        Dependency {
            issue_id: "issue".to_string(),
            depends_on_id: "dep".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            metadata: None,
            thread_id: None,
        }
    }

    #[test]
    fn dependency_validation_rejects_self_dependency() {
        let mut dep = base_dependency();
        dep.depends_on_id = "issue".to_string();
        let store = FakeStore {
            issue_exists: true,
            depends_on_exists: true,
            dependency_exists: false,
            would_cycle: false,
        };

        let err = DependencyValidator::validate(&dep, &store).unwrap_err();
        match err {
            BeadsError::Validation { field, .. } => assert_eq!(field, "depends_on_id"),
            _ => unreachable!("expected validation error"),
        }
    }

    #[test]
    fn dependency_validation_rejects_missing_issue() {
        let dep = base_dependency();
        let store = FakeStore {
            issue_exists: false,
            depends_on_exists: false,
            dependency_exists: false,
            would_cycle: false,
        };

        let err = DependencyValidator::validate(&dep, &store).unwrap_err();
        match err {
            BeadsError::ValidationErrors { errors } => {
                assert!(errors.iter().any(|e| e.field == "issue_id"));
                assert!(errors.iter().any(|e| e.field == "depends_on_id"));
            }
            _ => unreachable!("expected validation errors"),
        }
    }

    #[test]
    fn dependency_validation_rejects_cycle() {
        let dep = base_dependency();
        let store = FakeStore {
            issue_exists: true,
            depends_on_exists: true,
            dependency_exists: false,
            would_cycle: true,
        };

        let err = DependencyValidator::validate(&dep, &store).unwrap_err();
        match err {
            BeadsError::Validation { field, .. } => assert_eq!(field, "depends_on_id"),
            _ => unreachable!("expected validation error"),
        }
    }

    #[test]
    fn dependency_validation_allows_non_blocking_cycle() {
        let mut dep = base_dependency();
        dep.dep_type = DependencyType::Related;
        let store = FakeStore {
            issue_exists: true,
            depends_on_exists: true,
            dependency_exists: false,
            would_cycle: true,
        };

        assert!(DependencyValidator::validate(&dep, &store).is_ok());
    }

    struct DirectionalCycleStore {
        cycle_from: &'static str,
        cycle_to: &'static str,
    }

    impl DependencyStore for DirectionalCycleStore {
        fn issue_exists(&self, _id: &str) -> Result<bool, BeadsError> {
            Ok(true)
        }

        fn dependency_exists(
            &self,
            _issue_id: &str,
            _depends_on_id: &str,
        ) -> Result<bool, BeadsError> {
            Ok(false)
        }

        fn would_create_cycle(
            &self,
            issue_id: &str,
            depends_on_id: &str,
        ) -> Result<bool, BeadsError> {
            Ok(issue_id == self.cycle_from && depends_on_id == self.cycle_to)
        }
    }

    #[test]
    fn dependency_validation_reverses_parent_child_cycle_check() {
        let mut dep = base_dependency();
        dep.dep_type = DependencyType::ParentChild;
        let store = DirectionalCycleStore {
            cycle_from: "dep",
            cycle_to: "issue",
        };

        let err = DependencyValidator::validate(&dep, &store).unwrap_err();
        match err {
            BeadsError::Validation { field, .. } => assert_eq!(field, "depends_on_id"),
            _ => unreachable!("expected validation error"),
        }
    }

    #[test]
    fn dependency_validation_parent_child_ignores_standard_direction_cycle() {
        let mut dep = base_dependency();
        dep.dep_type = DependencyType::ParentChild;
        let store = DirectionalCycleStore {
            cycle_from: "issue",
            cycle_to: "dep",
        };

        assert!(DependencyValidator::validate(&dep, &store).is_ok());
    }

    #[test]
    fn dependency_validation_rejects_duplicate() {
        let dep = base_dependency();
        let store = FakeStore {
            issue_exists: true,
            depends_on_exists: true,
            dependency_exists: true,
            would_cycle: false,
        };

        let err = DependencyValidator::validate(&dep, &store).unwrap_err();
        match err {
            BeadsError::Validation { field, .. } => assert_eq!(field, "depends_on_id"),
            _ => unreachable!("expected validation error"),
        }
    }

    #[test]
    fn issue_validation_collects_multiple_errors() {
        let mut issue = base_issue();
        issue.id = String::new();
        issue.title = String::new();
        issue.priority = Priority(9);
        issue.updated_at = Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0).unwrap();

        let errors = IssueValidator::validate(&issue).unwrap_err();
        let fields: Vec<_> = errors.iter().map(|err| err.field.as_str()).collect();
        assert!(fields.contains(&"id"));
        assert!(fields.contains(&"title"));
        assert!(fields.contains(&"priority"));
        assert!(fields.contains(&"updated_at"));
    }

    #[test]
    fn issue_validation_rejects_external_ref_whitespace() {
        let mut issue = base_issue();
        issue.external_ref = Some("gh 12".to_string());

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "external_ref"));
    }

    #[test]
    fn id_format_validation_accepts_classic_ids() {
        assert!(is_valid_id_format("bd-abc123"));
        assert!(is_valid_id_format("beads9-0a9"));
    }

    #[test]
    fn id_format_validation_rejects_invalid_ids() {
        assert!(!is_valid_id_format("BD-abc123"));
        assert!(!is_valid_id_format("bd-ABC"));
        // 1 char hash is now allowed (min 1)
        assert!(is_valid_id_format("bd-1"));
        // 9 char hash is allowed (max 40 for hierarchical IDs)
        assert!(is_valid_id_format("bd-abc123456"));

        assert!(!is_valid_id_format("bd_abc"));
        assert!(!is_valid_id_format("bd-abc.def"));
        assert!(!is_valid_id_format("bd-abc.1a"));

        // 26 char hash is now valid (within max 40)
        assert!(is_valid_id_format("bd-abc12345678901234567890123456"));

        // Too long (41 chars) - exceeds max 40
        assert!(!is_valid_id_format(
            "bd-abc123456789012345678901234567890123456789"
        ));
    }

    #[test]
    fn id_format_validation_accepts_long_hash() {
        // Fallback generates 12+ chars. Should be accepted.
        assert!(is_valid_id_format("bd-abc123456789"));
    }

    // =========================================================================
    // SYNC SAFETY VALIDATOR TESTS
    // =========================================================================

    #[test]
    fn sync_safety_rejects_git_path_component() {
        use std::path::PathBuf;

        // Direct .git directory
        let git_path = PathBuf::from("/project/.git/config");
        let result = SyncSafetyValidator::validate_no_git_path(&git_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains(".git"));

        // .git as intermediate component
        let git_path2 = PathBuf::from("/project/.git/objects/pack");
        assert!(SyncSafetyValidator::validate_no_git_path(&git_path2).is_err());
    }

    #[test]
    fn sync_safety_allows_beads_path() {
        use std::path::PathBuf;

        let beads_path = PathBuf::from("/project/.beads/issues.jsonl");
        let result = SyncSafetyValidator::validate_no_git_path(&beads_path);
        assert!(result.is_ok());
    }

    #[test]
    fn sync_safety_allows_gitignore_file() {
        use std::path::PathBuf;

        // .gitignore is NOT .git - should be allowed
        let gitignore_path = PathBuf::from("/project/.gitignore");
        let result = SyncSafetyValidator::validate_no_git_path(&gitignore_path);
        assert!(result.is_ok());
    }

    #[test]
    fn sync_safety_rejects_git_in_string() {
        use std::path::PathBuf;

        // Paths ending with .git
        let git_path = PathBuf::from("/project/.git");
        assert!(SyncSafetyValidator::validate_no_git_path(&git_path).is_err());

        // Path with /.git/ in middle
        let git_path2 = PathBuf::from("/repo/.git/hooks/pre-commit");
        assert!(SyncSafetyValidator::validate_no_git_path(&git_path2).is_err());
    }

    #[test]
    fn sync_safety_containment_rejects_escape() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();

        // Path outside beads_dir
        let outside_path = temp.path().join("src/main.rs");
        let result =
            SyncSafetyValidator::validate_path_containment(&outside_path, &beads_dir, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("outside allowed directory")
        );
    }

    #[test]
    fn sync_safety_containment_allows_beads_subpath() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();

        // Create the file so canonicalize works
        let jsonl_path = beads_dir.join("issues.jsonl");
        std::fs::write(&jsonl_path, "").unwrap();

        let result = SyncSafetyValidator::validate_path_containment(&jsonl_path, &beads_dir, false);
        assert!(result.is_ok());
    }

    #[test]
    fn sync_safety_containment_allows_external_with_flag() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");

        // Path outside beads_dir but external allowed
        let outside_path = temp.path().join("external.jsonl");
        let result =
            SyncSafetyValidator::validate_path_containment(&outside_path, &beads_dir, true);
        assert!(result.is_ok());
    }

    #[test]
    fn sync_safety_containment_rejects_git_even_with_external_flag() {
        use std::path::PathBuf;

        let beads_dir = PathBuf::from("/project/.beads");
        let git_path = PathBuf::from("/project/.git/config");

        // Even with allow_external=true, .git should be rejected
        let result = SyncSafetyValidator::validate_path_containment(&git_path, &beads_dir, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains(".git"));
    }

    #[test]
    fn sync_safety_source_scan_accepts_complete_real_tree() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        SyncSafetyValidator::validate_no_git_authority_in_sync_sources(repo_root)
            .expect("the complete sync source boundary must be authority-free");
    }

    #[test]
    fn sync_safety_source_scan_rejects_missing_boundary() {
        let temp = tempfile::TempDir::new().unwrap();
        let error = SyncSafetyValidator::validate_no_git_authority_in_sync_sources(temp.path())
            .expect_err("a missing source boundary must fail closed");
        assert_eq!(error.field, "sync_source");
        assert!(error.message.contains("cannot inspect required directory"));
    }

    #[test]
    fn sync_safety_source_scan_rejects_direct_command_construction() {
        let source = r#"fn probe(tool: &str) {
            let _child = std
                :: process
                :: Command
                :: new(tool)
                .spawn();
        }"#;
        let error =
            SyncSafetyValidator::validate_sync_source_text(Path::new("src/sync/probe.rs"), source)
                .expect_err("indirect program selection still grants process authority");
        assert!(error.message.contains("subprocess command construction"));
        assert!(error.message.contains("src/sync/probe.rs"));
    }

    #[test]
    fn sync_safety_source_scan_rejects_vcs_wrapper_delegation() {
        let source = r#"fn status() {
            crate::cli::commands::vcs::execute_for_sync();
        }"#;
        let error = SyncSafetyValidator::validate_sync_source_text(
            Path::new("src/cli/commands/sync.rs"),
            source,
        )
        .expect_err("sync must not regain authority through a VCS wrapper");
        assert!(error.message.contains("process-capable CLI adapter"));
        assert!(error.message.contains("src/cli/commands/sync.rs"));
    }

    #[test]
    fn sync_safety_source_scan_rejects_inclusion_escape_hatches() {
        for (source, marker) in [
            (
                r#"include!("../../../outside/authority.rs");"#,
                "source inclusion",
            ),
            (
                r#"const HIDDEN: &[u8] = include_bytes!("authority.rs");"#,
                "byte inclusion",
            ),
            (
                r#"#[path = "../../../outside/authority.rs"] mod hidden;"#,
                "module path inclusion",
            ),
        ] {
            let error = SyncSafetyValidator::validate_sync_source_text(
                Path::new("src/sync/escape.rs"),
                source,
            )
            .expect_err("inclusion escape hatch must fail closed");
            assert!(error.message.contains(marker), "{error:?}");
        }
    }

    #[test]
    fn sync_safety_source_scan_rejects_every_process_capable_cli_sibling() {
        for adapter in [
            "changelog",
            "config",
            "doctor",
            "orphans",
            "stats",
            "upgrade",
            "vcs",
            "version",
        ] {
            let source = format!("use super::{{safe_helper, {adapter}}};");
            let error = SyncSafetyValidator::validate_sync_source_text(
                Path::new("src/cli/commands/sync.rs"),
                &source,
            )
            .expect_err("process-capable sibling import must fail closed");
            assert!(
                error.message.contains("process-capable CLI sibling"),
                "{adapter}: {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn sync_safety_source_scan_rejects_symlinked_entries() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let sync_dir = temp.path().join("src/sync");
        let cli_dir = temp.path().join("src/cli/commands");
        std::fs::create_dir_all(&sync_dir).unwrap();
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(cli_dir.join("sync.rs"), "").unwrap();
        std::fs::write(temp.path().join("outside.rs"), "").unwrap();
        symlink(temp.path().join("outside.rs"), sync_dir.join("hidden.rs")).unwrap();

        let error = SyncSafetyValidator::validate_no_git_authority_in_sync_sources(temp.path())
            .expect_err("symlinked source entries must fail closed");
        assert!(error.message.contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn sync_safety_source_scan_rejects_fifo_entries() {
        let temp = tempfile::TempDir::new().unwrap();
        let sync_dir = temp.path().join("src/sync");
        let cli_dir = temp.path().join("src/cli/commands");
        std::fs::create_dir_all(&sync_dir).unwrap();
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(sync_dir.join("mod.rs"), "").unwrap();
        std::fs::write(cli_dir.join("sync.rs"), "").unwrap();

        let fifo = sync_dir.join("authority.rs");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available for this Unix regression");
        assert!(status.success(), "mkfifo fixture setup failed: {status}");

        let error = SyncSafetyValidator::validate_no_git_authority_in_sync_sources(temp.path())
            .expect_err("special source entries must fail closed");
        assert_eq!(error.field, "sync_source");
        assert!(error.message.contains("unsupported special entry"));
        assert!(error.message.contains("authority.rs"));
    }

    #[test]
    fn sync_safety_source_scan_rejects_non_utf8_rust_source() {
        let temp = tempfile::TempDir::new().unwrap();
        let sync_dir = temp.path().join("src/sync");
        let cli_dir = temp.path().join("src/cli/commands");
        std::fs::create_dir_all(&sync_dir).unwrap();
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(sync_dir.join("mod.rs"), b"pub fn safe() {}\n").unwrap();
        std::fs::write(sync_dir.join("invalid.rs"), b"pub fn invalid() {\xff}\n").unwrap();
        std::fs::write(cli_dir.join("sync.rs"), "").unwrap();

        let error = SyncSafetyValidator::validate_no_git_authority_in_sync_sources(temp.path())
            .expect_err("non-UTF-8 Rust source must fail closed");
        assert_eq!(error.field, "sync_source");
        assert!(error.message.contains("required UTF-8 source"));
        assert!(error.message.contains("invalid.rs"));
    }

    fn validate_manifest_has_no_direct_runtime_git_dependencies(path: &Path) -> Result<(), String> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let manifest = toml::from_str::<toml::Value>(&source)
            .map_err(|error| format!("cannot parse {} as TOML: {error}", path.display()))?;
        let root = manifest
            .as_table()
            .ok_or_else(|| "root must be a TOML table".to_string())?;
        let workspace_dependencies = root
            .get("workspace")
            .and_then(toml::Value::as_table)
            .and_then(|workspace| workspace.get("dependencies"))
            .map(|dependencies| {
                dependencies
                    .as_table()
                    .ok_or_else(|| "root.workspace.dependencies must be a TOML table".to_string())
            })
            .transpose()?;

        if let Some(dependencies) = root.get("dependencies") {
            inspect_runtime_dependency_table(
                dependencies,
                "root.dependencies",
                workspace_dependencies,
            )?;
        }
        if let Some(targets) = root.get("target") {
            let targets = targets
                .as_table()
                .ok_or_else(|| "root.target must be a TOML table".to_string())?;
            for (selector, target) in targets {
                let target = target
                    .as_table()
                    .ok_or_else(|| format!("root.target.{selector} must be a TOML table"))?;
                if let Some(dependencies) = target.get("dependencies") {
                    inspect_runtime_dependency_table(
                        dependencies,
                        &format!("root.target.{selector}.dependencies"),
                        workspace_dependencies,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn inspect_runtime_dependency_table(
        value: &toml::Value,
        location: &str,
        workspace_dependencies: Option<&toml::Table>,
    ) -> Result<(), String> {
        let dependencies = value
            .as_table()
            .ok_or_else(|| format!("{location} must be a TOML table"))?;
        for (alias, specification) in dependencies {
            let package =
                runtime_dependency_package(alias, specification, location, workspace_dependencies)?;
            if is_forbidden_git_library(alias) || is_forbidden_git_library(&package) {
                return Err(format!(
                    "{location}.{alias} grants forbidden Git library authority via package {package:?}"
                ));
            }
        }
        Ok(())
    }

    fn runtime_dependency_package(
        alias: &str,
        specification: &toml::Value,
        location: &str,
        workspace_dependencies: Option<&toml::Table>,
    ) -> Result<String, String> {
        match specification {
            toml::Value::String(_) => Ok(alias.to_string()),
            toml::Value::Table(table) => {
                let inherits_workspace = table
                    .get("workspace")
                    .map(|value| {
                        value.as_bool().ok_or_else(|| {
                            format!("{location}.{alias}.workspace must be a boolean")
                        })
                    })
                    .transpose()?
                    .unwrap_or(false);
                if inherits_workspace {
                    let inherited = workspace_dependencies
                        .and_then(|dependencies| dependencies.get(alias))
                        .ok_or_else(|| {
                            format!("{location}.{alias} inherits a missing workspace dependency")
                        })?;
                    return runtime_dependency_package(
                        alias,
                        inherited,
                        "root.workspace.dependencies",
                        None,
                    );
                }
                table
                    .get("package")
                    .map(|package| {
                        package
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| format!("{location}.{alias}.package must be a string"))
                    })
                    .transpose()
                    .map(|package| package.unwrap_or_else(|| alias.to_string()))
            }
            _ => Err(format!(
                "{location}.{alias} must be a version string or dependency table"
            )),
        }
    }

    fn is_forbidden_git_library(package: &str) -> bool {
        let normalized = package.to_ascii_lowercase().replace('_', "-");
        matches!(
            normalized.as_str(),
            "git2" | "gitoxide" | "gix" | "libgit2" | "git-repository"
        ) || ["git2-", "gitoxide-", "gix-", "libgit2-", "git-repository-"]
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
    }

    #[test]
    fn sync_safety_no_direct_runtime_git_library_dependencies() {
        // Anchor to the crate root: sibling tests may change the process CWD,
        // and a relative path would then resolve against their directory.
        validate_manifest_has_no_direct_runtime_git_dependencies(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("direct normal and target runtime dependencies must be Git-authority-free");

        for (name, manifest) in [
            ("root", "[dependencies]\ngix = \"1\"\n"),
            (
                "alias",
                "[dependencies]\nsafe_name = { package = \"git2\", version = \"1\" }\n",
            ),
            (
                "target",
                "[target.'cfg(unix)'.dependencies]\ngitoxide = \"1\"\n",
            ),
            (
                "workspace-inherited",
                "[dependencies]\ngit_backend = { workspace = true }\n\
                 [workspace.dependencies]\ngit_backend = { package = \"libgit2\", version = \"1\" }\n",
            ),
            (
                "native-sys",
                "[dependencies]\nlibgit2-sys = { version = \"1\" }\n",
            ),
            (
                "git2-adapter",
                "[dependencies]\ngit2_curl = { package = \"git2-curl\", version = \"1\" }\n",
            ),
            (
                "gix-family",
                "[dependencies]\nsafe_alias = { package = \"gix-worktree\", version = \"1\" }\n",
            ),
            (
                "legacy-gitoxide",
                "[dependencies]\ngit-repository = \"1\"\n",
            ),
        ] {
            let temp = tempfile::TempDir::new().expect("temp dir");
            let path = temp.path().join("Cargo.toml");
            std::fs::write(&path, manifest).expect("write manifest");
            let error = validate_manifest_has_no_direct_runtime_git_dependencies(&path)
                .expect_err("forbidden dependency must fail closed");
            assert!(error.contains("forbidden Git library"), "{name}: {error}");
        }

        let tooling_only = "\
            [build-dependencies]\n\
            vergen-gix = \"1\"\n\
            [dev-dependencies]\n\
            git2 = \"1\"\n\
            [workspace.dependencies]\n\
            gix = \"1\"\n";
        let temp = tempfile::TempDir::new().expect("temp dir");
        let path = temp.path().join("Cargo.toml");
        std::fs::write(&path, tooling_only).expect("write tooling-only manifest");
        validate_manifest_has_no_direct_runtime_git_dependencies(&path)
            .expect("build/dev declarations and unused workspace entries are not runtime edges");

        for allowed in [
            "git-version",
            "git-cliff-core",
            "github-actions",
            "digit2",
            "legit2",
        ] {
            assert!(
                !is_forbidden_git_library(allowed),
                "non-authority package {allowed:?} must not be rejected by family matching"
            );
        }
    }

    #[test]
    fn sync_safety_dependency_guard_fails_closed_on_unreadable_or_malformed_manifest() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let missing = temp.path().join("missing.toml");
        assert!(
            validate_manifest_has_no_direct_runtime_git_dependencies(&missing)
                .expect_err("missing manifest must fail closed")
                .contains("cannot read")
        );

        let directory = temp.path().join("directory.toml");
        std::fs::create_dir(&directory).expect("create directory fixture");
        assert!(
            validate_manifest_has_no_direct_runtime_git_dependencies(&directory)
                .expect_err("non-file manifest must fail closed")
                .contains("cannot read")
        );

        let malformed = temp.path().join("malformed.toml");
        std::fs::write(&malformed, "[dependencies\nbroken = true")
            .expect("write malformed manifest");
        assert!(
            validate_manifest_has_no_direct_runtime_git_dependencies(&malformed)
                .expect_err("malformed manifest must fail closed")
                .contains("cannot parse")
        );

        let invalid_form = temp.path().join("invalid-form.toml");
        std::fs::write(&invalid_form, "[dependencies]\ngix = [\"1\"]\n")
            .expect("write invalid dependency form");
        assert!(
            validate_manifest_has_no_direct_runtime_git_dependencies(&invalid_form)
                .expect_err("non-string dependency form must fail closed")
                .contains("must be a version string or dependency table")
        );
    }
}
