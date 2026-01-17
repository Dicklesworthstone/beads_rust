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
use crate::model::{Comment, Dependency, Issue, Priority};
use std::path::Path;

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

        // ID: Required, max 50 chars, prefix-hash format.
        if issue.id.trim().is_empty() {
            errors.push(ValidationError::new("id", "cannot be empty"));
        }
        if issue.id.len() > 50 {
            errors.push(ValidationError::new("id", "exceeds 50 characters"));
        }
        if !issue.id.is_empty() && !is_valid_id_format(&issue.id) {
            errors.push(ValidationError::new(
                "id",
                "invalid format (expected prefix-hash)",
            ));
        }

        // Title: Required, max 500 chars.
        if issue.title.trim().is_empty() {
            errors.push(ValidationError::new("title", "cannot be empty"));
        }
        if issue.title.len() > 500 {
            errors.push(ValidationError::new("title", "exceeds 500 characters"));
        }

        // Description: Optional, max 100KB.
        if let Some(description) = issue.description.as_ref() {
            if description.len() > 102_400 {
                errors.push(ValidationError::new("description", "exceeds 100KB"));
            }
        }

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

        // External reference: Optional, max 200 chars, no whitespace.
        if let Some(external_ref) = issue.external_ref.as_ref() {
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

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
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

        if dep.dep_type.is_blocking() && store.would_create_cycle(&dep.issue_id, &dep.depends_on_id)? {
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

        if comment.body.len() > 51_200 {
            errors.push(ValidationError::new("content", "exceeds 50KB"));
        }

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
    let mut parts = id.splitn(2, '-');
    let Some(prefix) = parts.next() else {
        return false;
    };
    let Some(hash) = parts.next() else {
        return false;
    };

    if prefix.is_empty() || prefix.len() > 10 {
        return false;
    }

    if !prefix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }

    if hash.len() < 3 || hash.len() > 8 {
        return false;
    }

    if !hash
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }

    true
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
/// These are enforced by design (no git dependencies) and by runtime validation.
pub struct SyncSafetyValidator;

impl SyncSafetyValidator {
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
            if let std::path::Component::Normal(name) = component {
                if name == ".git" {
                    return Err(ValidationError::new(
                        "path",
                        "sync operations cannot access .git directory (safety invariant)",
                    ));
                }
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
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let canonical_beads = beads_dir
            .canonicalize()
            .unwrap_or_else(|_| beads_dir.to_path_buf());

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

    /// Asserts that sync code paths don't execute git commands.
    ///
    /// This is a compile-time design assertion documented here for clarity:
    /// - No `std::process::Command::new("git")` in sync module
    /// - No git library dependencies (gitoxide, git2, etc.)
    /// - Verified by static analysis: `grep -r "Command::new.*git" src/sync/`
    ///
    /// At runtime, this function serves as documentation and can be used
    /// in tests to validate the invariant holds.
    #[inline]
    pub const fn assert_no_git_in_sync() {
        // This is a compile-time design assertion.
        // The actual enforcement is:
        // 1. No git dependencies in Cargo.toml for sync
        // 2. No Command::new("git") calls in src/sync/
        // 3. This is verified by tests and grep/audit
    }
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
    fn issue_validation_rejects_large_description() {
        let mut issue = base_issue();
        issue.description = Some("x".repeat(102_401));

        let errors = IssueValidator::validate(&issue).unwrap_err();
        assert!(errors.iter().any(|err| err.field == "description"));
    }

    #[test]
    fn label_validation_rejects_invalid_characters() {
        let err = LabelValidator::validate("bad label").unwrap_err();
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
            _ => panic!("expected validation error"),
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
            _ => panic!("expected validation errors"),
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
            _ => panic!("expected validation error"),
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
            _ => panic!("expected validation error"),
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
        assert!(!is_valid_id_format("bd-1"));
        assert!(!is_valid_id_format("bd-abc123456"));
        assert!(!is_valid_id_format("bd_abc"));
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

    /// This test verifies the core safety invariant: no git commands in sync code.
    ///
    /// It uses static analysis (grep) to prove that `Command::new("git")` does
    /// not appear in the sync module.
    #[test]
    fn sync_safety_no_git_commands_in_sync_module() {
        use std::process::Command;

        // Search for git command invocations in sync module
        let output = Command::new("grep")
            .args(["-r", "Command::new.*git", "src/sync/"])
            .output();

        match output {
            Ok(result) => {
                // grep returns exit code 1 when no matches found (which is what we want)
                // grep returns exit code 0 when matches found (which is a failure)
                let stdout = String::from_utf8_lossy(&result.stdout);
                assert!(
                    result.status.code() == Some(1) || stdout.is_empty(),
                    "SAFETY VIOLATION: Found git commands in sync module:\n{stdout}"
                );
            }
            Err(_) => {
                // If grep isn't available, skip this test with a warning
                // This can happen in some CI environments
                eprintln!("Warning: grep not available, skipping static analysis test");
            }
        }
    }

    /// Verify no runtime git dependencies exist in Cargo.toml [dependencies] section.
    ///
    /// Note: Build-time dependencies (like vergen-gix) are allowed since they
    /// don't affect sync runtime behavior.
    #[test]
    fn sync_safety_no_git_library_dependencies() {
        let cargo_toml = std::fs::read_to_string("Cargo.toml").unwrap_or_default();

        // Extract only the [dependencies] section (not [build-dependencies] or [dev-dependencies])
        let deps_section = cargo_toml
            .lines()
            .skip_while(|line| !line.starts_with("[dependencies]"))
            .skip(1) // Skip the [dependencies] header
            .take_while(|line| !line.starts_with('[')) // Stop at next section
            .collect::<Vec<_>>()
            .join("\n");

        // Check for common git library crates in runtime dependencies only
        let git_crates = ["git2 ", "gitoxide ", "gix ", "libgit2 "];

        for crate_name in git_crates {
            let crate_name = crate_name.trim();
            assert!(
                !deps_section.contains(crate_name),
                "SAFETY VIOLATION: Found git library dependency '{crate_name}' in runtime [dependencies]"
            );
        }
    }
}
