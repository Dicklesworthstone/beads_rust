//! Path validation and allowlist enforcement for sync operations.
//!
//! This module defines the explicit allowlist of files that `br sync` is permitted
//! to touch and provides validation functions to enforce this boundary.
//!
//! # Safety Model
//!
//! The sync allowlist is a critical safety boundary. All sync I/O operations MUST
//! pass through `validate_sync_path()` before performing any file operations.
//!
//! # Allowlist
//!
//! The following paths are permitted for sync operations:
//!
//! | Pattern | Purpose |
//! |---------|---------|
//! | `.beads/*.db` | `SQLite` database files |
//! | `.beads/*.db-wal` | `SQLite` WAL files |
//! | `.beads/*.db-shm` | `SQLite` shared memory files |
//! | `.beads/*.jsonl` | `JSONL` export files |
//! | `.beads/*.jsonl.tmp` | Temp files for atomic writes |
//! | `.beads/.manifest.json` | Export manifest |
//! | `.beads/metadata.json` | Workspace metadata |
//!
//! # External JSONL Paths
//!
//! The `BEADS_JSONL` environment variable can override the JSONL path.
//! When set to a path outside `.beads/`, sync will refuse to operate unless
//! `--allow-external-jsonl` is explicitly provided.
//!
//! # Git Path Safety
//!
//! Sync operations NEVER access `.git/` directories. This is a hard safety invariant
//! enforced by `validate_no_git_path()`. Even with `--allow-external-jsonl`, git
//! paths are always rejected.
//!
//! # References
//!
//! - `SYNC_SAFETY_INVARIANTS.md`: PC-1, PC-2, PC-3, PC-4, NG-5, NG-6, NGI-1, NGI-3

use crate::error::{BeadsError, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Files explicitly allowed for sync operations within `.beads/`.
///
/// This list is exhaustive - any file not matching these patterns is rejected.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "db",        // SQLite database
    "db-wal",    // SQLite WAL
    "db-shm",    // SQLite shared memory
    "jsonl",     // JSONL export
    "jsonl.tmp", // Atomic write temp files
];

/// Files explicitly allowed by exact name within `.beads/`.
pub const ALLOWED_EXACT_NAMES: &[&str] = &[".manifest.json", "metadata.json"];

/// Result of path validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathValidation {
    /// Path is allowed for sync operations.
    Allowed,
    /// Path is outside the beads directory.
    OutsideBeadsDir { path: PathBuf, beads_dir: PathBuf },
    /// Path has a disallowed extension.
    DisallowedExtension { path: PathBuf, extension: String },
    /// Path contains traversal sequences (e.g., `..`).
    TraversalAttempt { path: PathBuf },
    /// Path is a symlink pointing outside the beads directory.
    SymlinkEscape { path: PathBuf, target: PathBuf },
    /// Path failed canonicalization.
    CanonicalizationFailed { path: PathBuf, error: String },
    /// Path targets git internals (.git directory).
    GitPathAttempt { path: PathBuf },
}

impl PathValidation {
    /// Returns true if the path is allowed.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Returns the rejection reason as a human-readable string.
    #[must_use]
    pub fn rejection_reason(&self) -> Option<String> {
        match self {
            Self::Allowed => None,
            Self::OutsideBeadsDir { path, beads_dir } => Some(format!(
                "Path '{}' is outside the beads directory '{}'",
                path.display(),
                beads_dir.display()
            )),
            Self::DisallowedExtension { path, extension } => Some(format!(
                "Path '{}' has disallowed extension '{}' (allowed: {:?})",
                path.display(),
                extension,
                ALLOWED_EXTENSIONS
            )),
            Self::TraversalAttempt { path } => Some(format!(
                "Path '{}' contains traversal sequences",
                path.display()
            )),
            Self::SymlinkEscape { path, target } => Some(format!(
                "Symlink '{}' points outside beads directory to '{}'",
                path.display(),
                target.display()
            )),
            Self::CanonicalizationFailed { path, error } => Some(format!(
                "Failed to canonicalize path '{}': {}",
                path.display(),
                error
            )),
            Self::GitPathAttempt { path } => Some(format!(
                "Path '{}' targets git internals - sync never accesses .git/ (safety invariant NGI-3)",
                path.display()
            )),
        }
    }
}

/// Validates that a path does not target git internals.
///
/// This is a hard safety invariant: sync operations NEVER access `.git/` directories.
/// This check runs regardless of `allow_external` settings.
///
/// # Safety Invariants
///
/// - NGI-1: br sync NEVER executes git subprocess commands
/// - NGI-3: br sync NEVER modifies .git/ directory
///
/// # Returns
///
/// * `PathValidation::Allowed` if path does not target git
/// * `PathValidation::GitPathAttempt` if path contains `.git` component
#[must_use]
pub fn validate_no_git_path(path: &Path) -> PathValidation {
    fn has_git_component(candidate: &Path) -> bool {
        for component in candidate.components() {
            if let std::path::Component::Normal(name) = component {
                if name == ".git" {
                    return true;
                }
            }
        }

        let path_str = candidate.to_string_lossy();
        path_str.contains("/.git/")
            || path_str.contains("\\.git\\")
            || path_str.ends_with("/.git")
            || path_str.ends_with("\\.git")
    }

    // Check raw path first
    if has_git_component(path) {
        return PathValidation::GitPathAttempt {
            path: path.to_path_buf(),
        };
    }

    // Resolve the canonical path when possible (catches symlinks to .git)
    if let Ok(canonical) = path.canonicalize() {
        if has_git_component(&canonical) {
            return PathValidation::GitPathAttempt { path: canonical };
        }
    } else if let Some(parent) = path.parent() {
        if let Ok(canonical_parent) = parent.canonicalize() {
            if has_git_component(&canonical_parent) {
                return PathValidation::GitPathAttempt {
                    path: canonical_parent,
                };
            }
        }
    }

    PathValidation::Allowed
}

/// Validates that a path is allowed for sync operations.
///
/// # Arguments
///
/// * `path` - The path to validate
/// * `beads_dir` - The `.beads` directory path (must be absolute)
///
/// # Returns
///
/// * `PathValidation::Allowed` if the path is permitted
/// * Other variants describing why the path was rejected
///
/// # Logging
///
/// - DEBUG: Logs successful validation with path details
/// - WARN: Logs rejected paths with reason
///
/// # Example
///
/// ```ignore
/// let beads_dir = PathBuf::from("/project/.beads");
/// let result = validate_sync_path(&beads_dir.join("issues.jsonl"), &beads_dir);
/// assert!(result.is_allowed());
/// ```
#[allow(clippy::too_many_lines)]
pub fn validate_sync_path(path: &Path, beads_dir: &Path) -> PathValidation {
    // Log the validation attempt
    debug!(path = %path.display(), beads_dir = %beads_dir.display(), "Validating sync path");

    // CRITICAL: Check for git path access first (hard invariant - NGI-3)
    let git_check = validate_no_git_path(path);
    if !git_check.is_allowed() {
        warn!(
            path = %path.display(),
            reason = %git_check.rejection_reason().unwrap_or_default(),
            "Git path access blocked"
        );
        return git_check;
    }

    // Check for obvious traversal attempts in the raw path
    let path_str = path.to_string_lossy();
    if path_str.contains("..") {
        let result = PathValidation::TraversalAttempt {
            path: path.to_path_buf(),
        };
        warn!(
            path = %path.display(),
            reason = %result.rejection_reason().unwrap_or_default(),
            "Path validation rejected"
        );
        return result;
    }

    // Canonicalize the beads directory
    let canonical_beads = match beads_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            let result = PathValidation::CanonicalizationFailed {
                path: beads_dir.to_path_buf(),
                error: e.to_string(),
            };
            warn!(
                path = %beads_dir.display(),
                error = %e,
                "Beads directory canonicalization failed"
            );
            return result;
        }
    };

    // For new files that don't exist yet, we check the parent directory
    let path_to_check = if path.exists() {
        path.to_path_buf()
    } else {
        // For non-existent files, verify the parent exists and is valid
        match path.parent() {
            Some(parent) if parent.exists() => parent.to_path_buf(),
            _ => {
                // If parent doesn't exist, just check if the path would be under beads_dir
                if let Ok(relative) = path.strip_prefix(&canonical_beads) {
                    // Path is specified relative to beads_dir
                    if !relative.to_string_lossy().contains("..") {
                        return validate_extension_and_name(path);
                    }
                }
                // Otherwise, try to check as-is
                path.to_path_buf()
            }
        }
    };

    // Canonicalize the path (or its parent for new files)
    let canonical_path = match path_to_check.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            // For non-existent files, we can't canonicalize, so check prefix
            if !path.exists() {
                // Check if the path starts with the beads directory
                if path.starts_with(beads_dir) || path.starts_with(&canonical_beads) {
                    return validate_extension_and_name(path);
                }
            }
            let result = PathValidation::CanonicalizationFailed {
                path: path.to_path_buf(),
                error: e.to_string(),
            };
            warn!(
                path = %path.display(),
                error = %e,
                "Path canonicalization failed"
            );
            return result;
        }
    };

    // Check if the path is a symlink pointing outside beads_dir
    if path.is_symlink() {
        if let Ok(target) = std::fs::read_link(path) {
            let canonical_target = target.canonicalize().unwrap_or_else(|_| target.clone());
            if !canonical_target.starts_with(&canonical_beads) {
                let result = PathValidation::SymlinkEscape {
                    path: path.to_path_buf(),
                    target: canonical_target,
                };
                warn!(
                    path = %path.display(),
                    target = %target.display(),
                    "Symlink escape detected"
                );
                return result;
            }
        }
    }

    // Verify the path is under the beads directory
    // For existing files, use the canonical path; for new files, use the parent's canonical + filename
    let effective_canonical = if path.exists() {
        canonical_path
    } else {
        canonical_path.join(path.file_name().unwrap_or_default())
    };

    if !effective_canonical.starts_with(&canonical_beads) {
        let result = PathValidation::OutsideBeadsDir {
            path: path.to_path_buf(),
            beads_dir: canonical_beads,
        };
        warn!(
            path = %path.display(),
            beads_dir = %beads_dir.display(),
            reason = %result.rejection_reason().unwrap_or_default(),
            "Path validation rejected"
        );
        return result;
    }

    // Validate extension and name
    let extension_result = validate_extension_and_name(path);
    if !extension_result.is_allowed() {
        warn!(
            path = %path.display(),
            reason = %extension_result.rejection_reason().unwrap_or_default(),
            "Path validation rejected"
        );
        return extension_result;
    }

    debug!(path = %path.display(), "Path validated for sync I/O");
    PathValidation::Allowed
}

/// Validates that the file extension or name is in the allowlist.
fn validate_extension_and_name(path: &Path) -> PathValidation {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Check exact name matches first
    if ALLOWED_EXACT_NAMES.iter().any(|&name| file_name == name) {
        return PathValidation::Allowed;
    }

    // Check extension matches
    // Handle compound extensions like .jsonl.tmp
    for allowed_ext in ALLOWED_EXTENSIONS {
        if file_name.ends_with(&format!(".{allowed_ext}")) {
            return PathValidation::Allowed;
        }
    }

    // Extract simple extension for error message
    let extension = path
        .extension()
        .map_or_else(|| "none".to_string(), |e| e.to_string_lossy().to_string());

    PathValidation::DisallowedExtension {
        path: path.to_path_buf(),
        extension,
    }
}

/// Validates a path and returns an error if it's not allowed.
///
/// This is a convenience wrapper around `validate_sync_path` that returns
/// a `Result` for easier use in sync functions.
///
/// # Errors
///
/// Returns `BeadsError::Config` with a descriptive message if the path is not allowed.
pub fn require_valid_sync_path(path: &Path, beads_dir: &Path) -> Result<()> {
    let validation = validate_sync_path(path, beads_dir);
    match validation {
        PathValidation::Allowed => Ok(()),
        _ => Err(BeadsError::Config(
            validation
                .rejection_reason()
                .unwrap_or_else(|| "Path validation failed".to_string()),
        )),
    }
}

/// Checks if a path would be allowed for sync without logging.
///
/// This is useful for preflight checks where we want to validate paths
/// before attempting operations.
#[must_use]
pub fn is_sync_path_allowed(path: &Path, beads_dir: &Path) -> bool {
    // Quick check without full canonicalization for obvious cases
    let path_str = path.to_string_lossy();
    if path_str.contains("..") {
        return false;
    }

    // Check if path is under beads_dir and has allowed extension
    if path.starts_with(beads_dir) {
        return validate_extension_and_name(path).is_allowed();
    }

    // Full validation for edge cases
    validate_sync_path(path, beads_dir).is_allowed()
}

/// Validates a path for sync operations with optional external path support.
///
/// This is the main entry point for sync path validation. It enforces:
/// 1. Git paths are ALWAYS rejected (hard invariant)
/// 2. Paths outside `.beads/` require explicit `allow_external` opt-in
/// 3. External paths must still be valid JSONL files (not arbitrary files)
///
/// # Arguments
///
/// * `path` - The path to validate
/// * `beads_dir` - The `.beads` directory path
/// * `allow_external` - Whether to allow paths outside `.beads/`
///
/// # Errors
///
/// Returns `BeadsError::Config` with a descriptive message if validation fails.
///
/// # Examples
///
/// ```ignore
/// // Normal case: path inside .beads/
/// validate_sync_path_with_external(&path, &beads_dir, false)?;
///
/// // External JSONL with opt-in
/// validate_sync_path_with_external(&external_jsonl, &beads_dir, true)?;
/// ```
pub fn validate_sync_path_with_external(
    path: &Path,
    beads_dir: &Path,
    allow_external: bool,
) -> Result<()> {
    // CRITICAL: Git paths are ALWAYS rejected, even with allow_external
    let git_check = validate_no_git_path(path);
    if !git_check.is_allowed() {
        return Err(BeadsError::Config(
            git_check
                .rejection_reason()
                .unwrap_or_else(|| "Git path access denied".to_string()),
        ));
    }

    // If external paths are allowed, only validate file type (not containment)
    if allow_external {
        // Log the external path usage (safety invariant PC-2)
        tracing::info!(path = %path.display(), "Using external JSONL path (--allow-external-jsonl)");

        // Still validate it's a JSONL file
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Case-sensitive check is intentional: JSONL files should use lowercase .jsonl extension
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if !file_name.ends_with(".jsonl") && !file_name.ends_with(".jsonl.tmp") {
            return Err(BeadsError::Config(format!(
                "External path '{}' must be a .jsonl file",
                path.display()
            )));
        }

        // Check for traversal attempts even in external paths
        let path_str = path.to_string_lossy();
        if path_str.contains("..") {
            return Err(BeadsError::Config(format!(
                "Path '{}' contains traversal sequences",
                path.display()
            )));
        }

        return Ok(());
    }

    // Standard validation for paths within .beads/
    require_valid_sync_path(path, beads_dir)
}

/// Require that a path is safe for destructive sync operations (delete/overwrite).
///
/// This guard enforces the sync allowlist and ensures we never delete or overwrite
/// files outside `.beads/`, except for explicitly allowed external JSONL paths.
///
/// # Errors
///
/// Returns `BeadsError::Config` if the path is unsafe. Rejections are logged with
/// the attempted operation for auditability.
pub fn require_safe_sync_overwrite_path(
    path: &Path,
    beads_dir: &Path,
    allow_external: bool,
    operation: &str,
) -> Result<()> {
    let canonical_beads = beads_dir
        .canonicalize()
        .unwrap_or_else(|_| beads_dir.to_path_buf());
    let is_internal = path.starts_with(beads_dir) || path.starts_with(&canonical_beads);

    if is_internal {
        let validation = validate_sync_path(path, beads_dir);
        if validation.is_allowed() {
            debug!(
                path = %path.display(),
                operation,
                "Sync path approved for destructive operation"
            );
            return Ok(());
        }

        let reason = validation
            .rejection_reason()
            .unwrap_or_else(|| "Path validation failed".to_string());
        warn!(
            path = %path.display(),
            operation,
            reason = %reason,
            "Sync destructive path rejected"
        );
        return Err(BeadsError::Config(reason));
    }

    if !allow_external {
        let reason = format!("Refusing to {operation} outside .beads: {}", path.display());
        warn!(
            path = %path.display(),
            operation,
            reason = %reason,
            "Sync destructive path rejected"
        );
        return Err(BeadsError::Config(reason));
    }

    match validate_sync_path_with_external(path, beads_dir, true) {
        Ok(()) => {
            debug!(
                path = %path.display(),
                operation,
                "External sync path approved for destructive operation"
            );
            Ok(())
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                operation,
                error = %err,
                "Sync destructive path rejected"
            );
            Err(err)
        }
    }
}

/// Validates a temp file path for atomic write operations.
///
/// Temp files must:
/// 1. Be in the same directory as the target file (for atomic rename)
/// 2. Not target git internals
/// 3. Have the `.tmp` extension
///
/// # Errors
///
/// Returns `BeadsError::Config` if validation fails.
pub fn validate_temp_file_path(
    temp_path: &Path,
    target_path: &Path,
    beads_dir: &Path,
    allow_external: bool,
) -> Result<()> {
    // Git check is always enforced
    let git_check = validate_no_git_path(temp_path);
    if !git_check.is_allowed() {
        return Err(BeadsError::Config(
            git_check
                .rejection_reason()
                .unwrap_or_else(|| "Git path access denied".to_string()),
        ));
    }

    // Verify temp file is in the same directory as target (PC-4)
    let temp_parent = temp_path.parent();
    let target_parent = target_path.parent();

    if temp_parent != target_parent {
        return Err(BeadsError::Config(format!(
            "Temp file '{}' must be in the same directory as target '{}' (safety invariant PC-4)",
            temp_path.display(),
            target_path.display()
        )));
    }

    let has_tmp_extension = temp_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"));
    if !has_tmp_extension {
        return Err(BeadsError::Config(format!(
            "Temp file '{}' must use a .tmp extension",
            temp_path.display()
        )));
    }

    // If external is allowed for the target, it's allowed for the temp file too
    if allow_external {
        return Ok(());
    }

    // For internal paths, validate containment
    let canonical_beads = beads_dir
        .canonicalize()
        .unwrap_or_else(|_| beads_dir.to_path_buf());

    if let Some(parent) = temp_parent {
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if !canonical_parent.starts_with(&canonical_beads) {
            return Err(BeadsError::Config(format!(
                "Temp file '{}' is outside allowed directory '{}'",
                temp_path.display(),
                beads_dir.display()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_beads_dir() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("create temp dir");
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create beads dir");
        (temp, beads_dir)
    }

    #[test]
    fn test_allowed_jsonl_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, "{}").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "JSONL files should be allowed");
    }

    #[test]
    fn test_allowed_db_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("beads.db");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "DB files should be allowed");
    }

    #[test]
    fn test_allowed_db_wal_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("beads.db-wal");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "DB-WAL files should be allowed");
    }

    #[test]
    fn test_allowed_manifest_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join(".manifest.json");
        std::fs::write(&path, "{}").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "Manifest files should be allowed");
    }

    #[test]
    fn test_allowed_metadata_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("metadata.json");
        std::fs::write(&path, "{}").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "Metadata files should be allowed");
    }

    #[test]
    fn test_allowed_temp_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl.tmp");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "Temp JSONL files should be allowed");
    }

    #[test]
    fn test_rejected_outside_beads_dir() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let outside_path = beads_dir.parent().unwrap().join("outside.jsonl");
        std::fs::write(&outside_path, "").expect("write");

        let result = validate_sync_path(&outside_path, &beads_dir);
        assert!(
            matches!(result, PathValidation::OutsideBeadsDir { .. }),
            "Files outside beads dir should be rejected"
        );
    }

    #[test]
    fn test_rejected_traversal() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let traversal_path = beads_dir.join("../../../etc/passwd");

        let result = validate_sync_path(&traversal_path, &beads_dir);
        assert!(
            matches!(result, PathValidation::TraversalAttempt { .. }),
            "Traversal attempts should be rejected"
        );
    }

    #[test]
    fn test_rejected_disallowed_extension() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("config.yaml");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            matches!(result, PathValidation::DisallowedExtension { .. }),
            "Disallowed extensions should be rejected"
        );
    }

    #[test]
    fn test_rejected_source_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("main.rs");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            matches!(result, PathValidation::DisallowedExtension { .. }),
            "Source files should be rejected"
        );
    }

    #[test]
    fn test_rejected_absolute_path_outside() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = PathBuf::from("/etc/passwd");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            !result.is_allowed(),
            "Absolute paths outside beads dir should be rejected"
        );
    }

    #[test]
    fn test_new_file_in_beads_dir() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        // File doesn't exist yet but is in beads_dir with allowed extension
        let path = beads_dir.join("new.jsonl");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            result.is_allowed(),
            "New JSONL files in beads dir should be allowed"
        );
    }

    #[test]
    fn test_require_valid_sync_path_ok() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, "").expect("write");

        let result = require_valid_sync_path(&path, &beads_dir);
        assert!(result.is_ok(), "Valid paths should return Ok");
    }

    #[test]
    fn test_require_valid_sync_path_error() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("../../../etc/passwd");

        let result = require_valid_sync_path(&path, &beads_dir);
        assert!(result.is_err(), "Invalid paths should return Err");
        assert!(result.unwrap_err().to_string().contains("traversal"));
    }

    #[test]
    fn test_is_sync_path_allowed_quick_check() {
        let (_temp, beads_dir) = setup_test_beads_dir();

        assert!(is_sync_path_allowed(
            &beads_dir.join("issues.jsonl"),
            &beads_dir
        ));
        assert!(!is_sync_path_allowed(
            &beads_dir.join("../evil.jsonl"),
            &beads_dir
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_rejected() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp dir");
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create a target outside beads dir
        let outside_target = temp.path().join("secret.txt");
        std::fs::write(&outside_target, "secret data").expect("write");

        // Create symlink inside beads dir pointing outside
        let symlink_path = beads_dir.join("evil.jsonl");
        symlink(&outside_target, &symlink_path).expect("create symlink");

        let result = validate_sync_path(&symlink_path, &beads_dir);
        assert!(
            matches!(result, PathValidation::SymlinkEscape { .. }),
            "Symlinks escaping beads dir should be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_no_git_path_rejects_symlinked_git_parent() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp dir");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git dir");

        let symlink_parent = temp.path().join("gitlink");
        symlink(&git_dir, &symlink_parent).expect("create git symlink");

        let candidate = symlink_parent.join("issues.jsonl");
        let result = validate_no_git_path(&candidate);
        assert!(
            matches!(result, PathValidation::GitPathAttempt { .. }),
            "Symlinked parents targeting .git should be rejected"
        );
    }

    #[test]
    fn test_validation_logs_rejection() {
        // This test verifies the logging behavior by checking the return value
        // which includes the reason that would be logged
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("../../../etc/passwd");

        let result = validate_sync_path(&path, &beads_dir);
        let reason = result.rejection_reason();
        assert!(reason.is_some(), "Rejected paths should have a reason");
        assert!(
            reason.unwrap().contains("traversal"),
            "Reason should mention traversal"
        );
    }

    #[test]
    fn test_safe_overwrite_blocks_external_without_flag() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let path = temp.path().join("outside.jsonl");

        let result = require_safe_sync_overwrite_path(&path, &beads_dir, false, "overwrite");
        assert!(
            result.is_err(),
            "External overwrite should be rejected without flag"
        );
    }

    #[test]
    fn test_safe_overwrite_allows_external_jsonl_with_flag() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let path = temp.path().join("outside.jsonl");

        let result = require_safe_sync_overwrite_path(&path, &beads_dir, true, "overwrite");
        assert!(
            result.is_ok(),
            "External JSONL overwrite should be allowed with flag"
        );
    }

    #[test]
    fn test_safe_overwrite_rejects_external_non_jsonl() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let path = temp.path().join("outside.txt");

        let result = require_safe_sync_overwrite_path(&path, &beads_dir, true, "overwrite");
        assert!(
            result.is_err(),
            "External non-JSONL overwrite should be rejected"
        );
    }

    #[test]
    fn test_safe_overwrite_allows_manifest_inside_beads() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join(".manifest.json");

        let result = require_safe_sync_overwrite_path(&path, &beads_dir, true, "overwrite");
        assert!(
            result.is_ok(),
            "Manifest overwrite should be allowed inside .beads"
        );
    }
}
