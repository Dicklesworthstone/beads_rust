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
//! | `.beads/*.db-journal` | `SQLite` rollback journals |
//! | `.beads/*.db-fsqlite-ns-gate` | fsqlite multi-process namespace gate |
//! | `.beads/*.db-fsqlite-ns-use` | fsqlite multi-process namespace use-count |
//! | `.beads/*.jsonl` | `JSONL` export files |
//! | `.beads/*.jsonl.tmp` | Temp files for atomic writes |
//! | `.beads/*.jsonl.<pid>.tmp` | PID-scoped temp files for atomic writes |
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
use std::fs::File;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Files explicitly allowed for sync operations within `.beads/`.
///
/// This list is exhaustive - any file not matching these patterns is rejected.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "db",                 // SQLite database
    "db-wal",             // SQLite WAL
    "db-shm",             // SQLite shared memory
    "db-journal",         // SQLite rollback journal
    "db-fsqlite-ns-gate", // fsqlite multi-process namespace gate
    "db-fsqlite-ns-use",  // fsqlite multi-process namespace use-count
    "jsonl",              // JSONL export
    "jsonl.tmp",          // Atomic write temp files (plus pid-scoped .jsonl.<pid>.tmp)
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
    /// Path exists but is not a regular file.
    NonRegularFile { path: PathBuf },
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
                "Path '{}' has disallowed extension '{}' (allowed: {:?}, plus pid-scoped '*.jsonl.<pid>.tmp')",
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
            Self::NonRegularFile { path } => {
                Some(format!("Path '{}' must be a regular file", path.display()))
            }
            Self::GitPathAttempt { path } => Some(format!(
                "Path '{}' targets git internals - sync never accesses .git/ (safety invariant NGI-3)",
                path.display()
            )),
        }
    }
}

fn normalize_path_lexically(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }

    Some(normalized)
}

fn symlink_escape_for_existing_ancestor(
    path: &Path,
    canonical_beads: &Path,
) -> Option<PathValidation> {
    for ancestor in path.ancestors() {
        let Ok(metadata) = std::fs::symlink_metadata(ancestor) else {
            continue;
        };

        if !metadata.file_type().is_symlink() {
            continue;
        }

        let target = std::fs::read_link(ancestor)
            .map(|target| resolve_symlink_target_for_validation(ancestor, &target))
            .unwrap_or_else(|_| ancestor.to_path_buf());
        if !target.starts_with(canonical_beads) {
            return Some(PathValidation::SymlinkEscape {
                path: ancestor.to_path_buf(),
                target,
            });
        }
    }

    None
}

fn resolve_symlink_target_for_validation(link_path: &Path, target: &Path) -> PathBuf {
    let anchored = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(target)
    };
    let normalized = normalize_path_lexically(&anchored).unwrap_or(anchored);
    dunce::canonicalize(&normalized).unwrap_or(normalized)
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
            if let std::path::Component::Normal(name) = component
                && name == ".git"
            {
                return true;
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

    // Resolve each existing ancestor. The final path or its immediate parent
    // may not exist yet, but a higher symlinked ancestor can still target .git.
    for ancestor in path.ancestors() {
        let Ok(canonical_ancestor) = dunce::canonicalize(ancestor) else {
            continue;
        };
        if has_git_component(&canonical_ancestor) {
            return PathValidation::GitPathAttempt {
                path: canonical_ancestor,
            };
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

    let had_parent_dir = path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir));
    let Some(normalized_path) = normalize_path_lexically(path) else {
        let result = PathValidation::TraversalAttempt {
            path: path.to_path_buf(),
        };
        warn!(
            path = %path.display(),
            reason = %result.rejection_reason().unwrap_or_default(),
            "Path validation rejected"
        );
        return result;
    };

    // Canonicalize the beads directory
    let canonical_beads = match dunce::canonicalize(beads_dir) {
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

    if let Some(result) = symlink_escape_for_existing_ancestor(&normalized_path, &canonical_beads) {
        warn!(
            path = %path.display(),
            reason = %result.rejection_reason().unwrap_or_default(),
            "Path validation rejected"
        );
        return result;
    }

    if had_parent_dir
        && !normalized_path.starts_with(beads_dir)
        && !normalized_path.starts_with(&canonical_beads)
    {
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

    // For new files that don't exist yet, we check the parent directory
    let path_to_check = if normalized_path.exists() {
        normalized_path.clone()
    } else {
        // For non-existent files, verify the parent exists and is valid
        match normalized_path.parent() {
            Some(parent) if parent.exists() => parent.to_path_buf(),
            _ => {
                // If parent doesn't exist, just check if the path would be under beads_dir
                if let Ok(relative) = normalized_path.strip_prefix(&canonical_beads) {
                    // Path is specified relative to beads_dir
                    if !relative.to_string_lossy().contains("..") {
                        return validate_extension_and_name(&normalized_path);
                    }
                }
                // Otherwise, try to check as-is
                normalized_path.clone()
            }
        }
    };

    // Canonicalize the path (or its parent for new files)
    let canonical_path = match dunce::canonicalize(&path_to_check) {
        Ok(p) => p,
        Err(e) => {
            // For non-existent files, we can't canonicalize, so check prefix
            if !normalized_path.exists() {
                // Check if the path starts with the beads directory
                if normalized_path.starts_with(beads_dir)
                    || normalized_path.starts_with(&canonical_beads)
                {
                    return validate_extension_and_name(&normalized_path);
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
    if normalized_path.is_symlink()
        && let Ok(target) = std::fs::read_link(&normalized_path)
    {
        let canonical_target = resolve_symlink_target_for_validation(&normalized_path, &target);
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

    if normalized_path.exists() {
        match std::fs::symlink_metadata(&normalized_path) {
            Ok(metadata) if !metadata.is_file() => {
                let result = PathValidation::NonRegularFile {
                    path: path.to_path_buf(),
                };
                warn!(
                    path = %path.display(),
                    reason = %result.rejection_reason().unwrap_or_default(),
                    "Path validation rejected"
                );
                return result;
            }
            Ok(_) => {}
            Err(e) => {
                let result = PathValidation::CanonicalizationFailed {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                };
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Path metadata lookup failed"
                );
                return result;
            }
        }
    }

    // Verify the path is under the beads directory
    // For existing files, use the canonical path; for new files, use the parent's canonical + filename
    let effective_canonical = if normalized_path.exists() {
        canonical_path
    } else {
        canonical_path.join(normalized_path.file_name().unwrap_or_default())
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
    let extension_result = validate_extension_and_name(&normalized_path);
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

    if is_allowed_jsonl_temp_name(&file_name) {
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

fn is_allowed_jsonl_temp_name(file_name: &str) -> bool {
    if file_name.ends_with(".jsonl.tmp") {
        return true;
    }

    let Some(prefix) = file_name.strip_suffix(".tmp") else {
        return false;
    };
    let Some((base, pid)) = prefix.rsplit_once(".jsonl.") else {
        return false;
    };

    !base.is_empty() && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())
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
    let Some(normalized_path) = normalize_path_lexically(path) else {
        return false;
    };

    validate_sync_path(&normalized_path, beads_dir).is_allowed()
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

    // If a path still points at `.beads/`, keep the stricter internal
    // allowlist and symlink-escape checks even when external JSONL is enabled.
    let canonical_beads =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());
    let resolved_path = if path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let is_internal = path.starts_with(beads_dir)
        || path.starts_with(&canonical_beads)
        || resolved_path.starts_with(beads_dir)
        || resolved_path.starts_with(&canonical_beads);

    if is_internal {
        return require_valid_sync_path(path, beads_dir);
    }

    // If external paths are allowed, only validate file type (not containment).
    if allow_external {
        // Log the external path usage (safety invariant PC-2)
        tracing::info!(path = %path.display(), "Using external JSONL path (--allow-external-jsonl)");
        return validate_external_jsonl_path(path);
    }

    // Standard validation for paths within .beads/
    require_valid_sync_path(path, beads_dir)
}

fn validate_external_jsonl_path(path: &Path) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Case-sensitive check is intentional: JSONL files should use lowercase .jsonl extension
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if !file_name.ends_with(".jsonl") && !is_allowed_jsonl_temp_name(&file_name) {
        return Err(BeadsError::Config(format!(
            "External path '{}' must be a .jsonl file",
            path.display()
        )));
    }

    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(BeadsError::Config(format!(
                "Path '{}' contains traversal sequences",
                path.display()
            )));
        }
    }

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(BeadsError::Config(format!(
                "External path '{}' must not be a symlink",
                path.display()
            )));
        }
        if !metadata.is_file() {
            return Err(BeadsError::Config(format!(
                "External path '{}' must be a regular file",
                path.display()
            )));
        }
    }

    Ok(())
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
    let canonical_beads =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());

    // Resolve relative paths against cwd so that `.beads/issues.jsonl.<pid>.tmp`
    // is correctly recognized as internal when beads_dir is absolute (#238).
    let resolved_path = if path.is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let is_internal = resolved_path.starts_with(beads_dir)
        || resolved_path.starts_with(&canonical_beads)
        || path.starts_with(beads_dir)
        || path.starts_with(&canonical_beads);

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

    validate_sync_path_with_external(temp_path, beads_dir, allow_external)
}

/// Validate metadata on an already-opened file descriptor.
///
/// Pre-open path checks (`validate_sync_path`) race against the filesystem:
/// between validation and `File::open` a same-user attacker could swap the
/// path to a symlink, device, or FIFO. This function closes that TOCTOU gap
/// by inspecting the fd-level metadata (`fstat`) of the already-opened file.
///
/// # Errors
///
/// Returns `BeadsError::Config` if the opened file is not a regular file.
pub fn validate_jsonl_fd_metadata(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata().map_err(|err| {
        BeadsError::Config(format!(
            "Failed to read metadata on opened JSONL fd for {}: {err}",
            path.display()
        ))
    })?;

    if !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Opened fd for {} is not a regular file (possible TOCTOU swap after path validation)",
            path.display()
        )));
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
    fn test_allowed_db_journal_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("beads.db-journal");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(result.is_allowed(), "DB journal files should be allowed");
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
    fn test_allowed_normalized_internal_path_with_parent_component() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let subdir = temp.path().join("subdir");
        std::fs::create_dir_all(&subdir).expect("create subdir");
        std::fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues.jsonl");

        let path = subdir.join("..").join(".beads").join("issues.jsonl");
        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            result.is_allowed(),
            "Normalized in-tree paths should be allowed"
        );
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
    fn test_allowed_pid_scoped_temp_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl.12345.tmp");
        std::fs::write(&path, "").expect("write");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            result.is_allowed(),
            "PID-scoped temp JSONL files should be allowed"
        );
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
    fn test_rejected_directory_named_like_jsonl() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::create_dir_all(&path).expect("create directory");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            matches!(result, PathValidation::NonRegularFile { .. }),
            "Directories named like JSONL files should be rejected"
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
    fn test_rejected_git_path_component() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join(".git").join("config");

        let result = validate_sync_path(&path, &beads_dir);
        assert!(
            matches!(result, PathValidation::GitPathAttempt { .. }),
            ".git paths should be rejected"
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

    #[test]
    fn test_is_sync_path_allowed_accepts_normalized_internal_path() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let subdir = temp.path().join("subdir");
        std::fs::create_dir_all(&subdir).expect("create subdir");

        assert!(is_sync_path_allowed(
            &subdir.join("..").join(".beads").join("issues.jsonl"),
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
    fn test_relative_internal_symlink_is_not_misclassified_as_escape() {
        use std::os::unix::fs::symlink;

        let (_temp, beads_dir) = setup_test_beads_dir();
        let target_path = beads_dir.join("actual.jsonl");
        std::fs::write(&target_path, "{}\n").expect("write target");
        let symlink_path = beads_dir.join("linked.jsonl");
        symlink("actual.jsonl", &symlink_path).expect("create relative symlink");

        let result = validate_sync_path(&symlink_path, &beads_dir);

        assert!(
            matches!(result, PathValidation::NonRegularFile { .. }),
            "internal relative symlink should be rejected as non-regular, not as an escape: {result:?}"
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

    #[cfg(unix)]
    #[test]
    fn test_validate_no_git_path_rejects_missing_descendant_under_symlinked_git_parent() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp dir");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git dir");

        let symlink_parent = temp.path().join("gitlink");
        symlink(&git_dir, &symlink_parent).expect("create git symlink");

        let candidate = symlink_parent.join("missing").join("issues.jsonl");
        let result = validate_no_git_path(&candidate);
        assert!(
            matches!(result, PathValidation::GitPathAttempt { .. }),
            "Missing descendants under symlinked .git parents should be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_path_with_external_rejects_missing_descendant_under_symlinked_git_parent()
    {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp dir");
        let beads_dir = temp.path().join(".beads");
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&beads_dir).expect("create beads dir");
        std::fs::create_dir_all(&git_dir).expect("create .git dir");

        let symlink_parent = temp.path().join("gitlink");
        symlink(&git_dir, &symlink_parent).expect("create git symlink");

        let candidate = symlink_parent.join("missing").join("issues.jsonl");
        let result = validate_sync_path_with_external(&candidate, &beads_dir, true);
        assert!(
            result.is_err(),
            "External JSONL opt-in must not permit missing descendants under symlinked .git parents"
        );
        assert!(
            result.unwrap_err().to_string().contains("git"),
            "error should mention git path rejection"
        );
        assert!(
            !git_dir.join("missing").exists(),
            "validation must not create missing directories inside .git"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_path_with_external_rejects_symlinked_jsonl() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp dir");
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create beads dir");

        let outside_target = temp.path().join("secret.txt");
        std::fs::write(&outside_target, "secret data").expect("write");

        let symlink_path = temp.path().join("outside.jsonl");
        symlink(&outside_target, &symlink_path).expect("create symlink");

        let result = validate_sync_path_with_external(&symlink_path, &beads_dir, true);
        assert!(
            result.is_err(),
            "External symlinked JSONL paths should be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must not be a symlink"),
            "Error should explain why the external path was rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_path_with_external_keeps_internal_symlink_escape_checks() {
        use std::os::unix::fs::symlink;

        let (temp, beads_dir) = setup_test_beads_dir();
        let outside_dir = temp.path().join("outside");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        let symlink_parent = beads_dir.join("linked");
        symlink(&outside_dir, &symlink_parent).expect("create symlinked parent");

        let path = symlink_parent.join("issues.jsonl");
        let result = validate_sync_path_with_external(&path, &beads_dir, true);

        assert!(
            result.is_err(),
            "Internal-looking paths must not bypass .beads symlink-escape checks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sync_path_rejects_missing_descendant_under_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let (temp, beads_dir) = setup_test_beads_dir();
        let outside_dir = temp.path().join("outside");
        std::fs::create_dir_all(&outside_dir).expect("create outside dir");
        let symlink_parent = beads_dir.join("linked");
        symlink(&outside_dir, &symlink_parent).expect("create symlinked parent");

        let path = symlink_parent.join("nested").join("issues.jsonl");
        let result = validate_sync_path(&path, &beads_dir);

        assert!(
            matches!(result, PathValidation::SymlinkEscape { .. }),
            "Missing descendants below an escaping symlink parent must be rejected"
        );
        assert!(
            !outside_dir.join("nested").exists(),
            "validation must not create external parent directories"
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
    fn test_safe_overwrite_rejects_external_directory_named_jsonl() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let path = temp.path().join("outside.jsonl");
        std::fs::create_dir_all(&path).expect("create directory");

        let result = require_safe_sync_overwrite_path(&path, &beads_dir, true, "overwrite");
        assert!(
            result.is_err(),
            "External directories should be rejected even if they look like JSONL files"
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

    // =========================================================================
    // Tests for validate_temp_file_path (PC-4 safety invariant)
    // =========================================================================

    #[test]
    fn test_temp_file_valid_same_directory() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let target = beads_dir.join("issues.jsonl");
        let temp = beads_dir.join("issues.jsonl.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_ok(),
            "Temp file in same directory with .tmp extension should be valid"
        );
    }

    #[test]
    fn test_temp_file_valid_same_directory_with_pid_scoped_name() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let target = beads_dir.join("issues.jsonl");
        let temp = beads_dir.join("issues.jsonl.12345.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_ok(),
            "PID-scoped temp file in same directory should be valid"
        );
    }

    #[test]
    fn test_temp_file_rejects_different_directory() {
        let (temp_dir, beads_dir) = setup_test_beads_dir();
        let target = beads_dir.join("issues.jsonl");
        let temp = temp_dir.path().join("issues.jsonl.tmp"); // Parent dir, not beads_dir

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_err(),
            "Temp file in different directory should be rejected (PC-4)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("same directory") || err.contains("PC-4"),
            "Error should mention same directory requirement: {err}"
        );
    }

    #[test]
    fn test_temp_file_rejects_missing_tmp_extension() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let target = beads_dir.join("issues.jsonl");
        let temp = beads_dir.join("issues.jsonl.bak"); // Wrong extension

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_err(),
            "Temp file without .tmp extension should be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains(".tmp"),
            "Error should mention .tmp extension requirement: {err}"
        );
    }

    #[test]
    fn test_temp_file_rejects_git_path() {
        let (temp_dir, beads_dir) = setup_test_beads_dir();
        let git_dir = temp_dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git dir");
        let target = git_dir.join("config");
        let temp = git_dir.join("config.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, true);
        assert!(
            result.is_err(),
            "Temp file in .git directory should always be rejected"
        );
    }

    #[test]
    fn test_temp_file_allows_external_with_flag() {
        let (temp_dir, beads_dir) = setup_test_beads_dir();
        let external_dir = temp_dir.path().join("external");
        std::fs::create_dir_all(&external_dir).expect("create external dir");
        let target = external_dir.join("issues.jsonl");
        let temp = external_dir.join("issues.jsonl.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, true);
        assert!(
            result.is_ok(),
            "External temp file should be allowed when allow_external is true"
        );
    }

    #[test]
    fn test_temp_file_rejects_external_without_flag() {
        let (temp_dir, beads_dir) = setup_test_beads_dir();
        let external_dir = temp_dir.path().join("external");
        std::fs::create_dir_all(&external_dir).expect("create external dir");
        let target = external_dir.join("issues.jsonl");
        let temp = external_dir.join("issues.jsonl.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_err(),
            "External temp file should be rejected when allow_external is false"
        );
    }

    #[test]
    fn test_temp_file_nested_beads_subdir() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let subdir = beads_dir.join("history");
        std::fs::create_dir_all(&subdir).expect("create history subdir");
        let target = subdir.join("backup.jsonl");
        let temp = subdir.join("backup.jsonl.tmp");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_ok(),
            "Temp file in nested .beads subdir should be valid"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_temp_file_rejects_existing_symlink() {
        use std::os::unix::fs::symlink;

        let (temp_dir, beads_dir) = setup_test_beads_dir();
        let external_dir = temp_dir.path().join("external");
        std::fs::create_dir_all(&external_dir).expect("create external dir");
        let target = beads_dir.join("issues.jsonl");
        let temp = beads_dir.join("issues.jsonl.tmp");
        symlink(external_dir.join("capture.jsonl"), &temp).expect("create symlink");

        let result = validate_temp_file_path(&temp, &target, &beads_dir, false);
        assert!(
            result.is_err(),
            "Existing symlink temp paths should be rejected"
        );
    }

    #[test]
    fn test_validate_jsonl_fd_metadata_accepts_regular_file() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, "{}\n").expect("write");

        let file = File::open(&path).expect("open");
        assert!(
            validate_jsonl_fd_metadata(&file, &path).is_ok(),
            "regular file fd should pass metadata validation"
        );
    }

    #[test]
    fn test_validate_jsonl_fd_metadata_rejects_directory_fd() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let dir_path = beads_dir.join("subdir");
        std::fs::create_dir(&dir_path).expect("create dir");

        let file = File::open(&dir_path).expect("open directory");
        let result = validate_jsonl_fd_metadata(&file, &dir_path);
        assert!(
            result.is_err(),
            "directory fd should fail metadata validation"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not a regular file"),
            "error should mention regular file"
        );
    }

    // ========================================================================
    // beads_rust-yyxo: tests for sync-safety invariants under SYNC_SAFETY_INVARIANTS.md
    // PC-1, PC-3, PC-RECOVERY (added 2026-05-09 by audit-2026-05-09)
    // ========================================================================

    /// PC-1 / PC-3: a symlink inside `.beads/` whose canonicalized target
    /// escapes via `..` must be rejected as a SymlinkEscape, not silently
    /// accepted via lexical normalization. Linux-only because Windows
    /// symlink semantics differ.
    #[cfg(unix)]
    #[test]
    fn validate_sync_path_rejects_canonicalized_traversal() {
        use std::os::unix::fs::symlink;

        let (temp, beads_dir) = setup_test_beads_dir();
        // External target outside .beads/
        let external = temp.path().join("external");
        std::fs::create_dir_all(&external).expect("create external");
        let external_target = external.join("escape.jsonl");
        std::fs::write(&external_target, "{}").expect("write external");

        // Create a symlink inside .beads/ that points to the external file
        let symlink_path = beads_dir.join("issues.jsonl");
        symlink(&external_target, &symlink_path).expect("create escape symlink");

        let result = validate_sync_path(&symlink_path, &beads_dir);
        assert!(
            !result.is_allowed(),
            "symlink whose target escapes .beads/ must be rejected; got {result:?}"
        );
        assert!(
            matches!(result, PathValidation::SymlinkEscape { .. }),
            "expected SymlinkEscape, got {result:?}"
        );
    }

    /// PC-RECOVERY: paths under `.beads/.br_recovery/` are NOT directly
    /// validated by `validate_sync_path` (recovery has its own path
    /// validation in `src/config/mod.rs`). This test asserts the contract
    /// boundary: `.bak` extension is NOT in the sync-direct allowlist
    /// (it was never written through sync's path), but the test
    /// allowlist in `tests/e2e_sync_git_safety.rs` recognizes them as
    /// legitimate side-effect writes during sync invocation.
    #[test]
    fn validate_sync_path_does_not_accept_recovery_bak_directly() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let recovery = beads_dir.join(".br_recovery");
        std::fs::create_dir_all(&recovery).expect("create recovery dir");
        let bak = recovery.join("beads.db.20260101_000000_0.bak");
        std::fs::write(&bak, "").expect("write");

        let result = validate_sync_path(&bak, &beads_dir);
        assert!(
            !result.is_allowed(),
            "sync's validate_sync_path must NOT accept .bak (recovery owns its own path validation); got {result:?}"
        );
        assert!(
            matches!(result, PathValidation::DisallowedExtension { .. }),
            "expected DisallowedExtension, got {result:?}"
        );
    }

    /// PC-1 / PC-3: a path constructed to look like `.beads/.git/foo` must
    /// be rejected as `GitPathAttempt` regardless of whether `.beads/.git`
    /// exists or contains the actual repo. Hard invariant NGI-3.
    #[test]
    fn validate_sync_path_rejects_dotgit_under_beads() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let git_path = beads_dir.join(".git").join("HEAD");

        let result = validate_sync_path(&git_path, &beads_dir);
        assert!(
            !result.is_allowed(),
            ".beads/.git/* must always be rejected; got {result:?}"
        );
        assert!(
            matches!(result, PathValidation::GitPathAttempt { .. }),
            "expected GitPathAttempt, got {result:?}"
        );
    }

    /// PC-1: `validate_sync_path` (the in-tree validator) MUST reject
    /// arbitrary external paths even when `BEADS_JSONL`-style env vars
    /// are NOT in play. Use `validate_sync_path_with_external` when the
    /// caller has explicit external-jsonl authorization.
    #[test]
    fn validate_sync_path_rejects_absolute_external_path() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let external = temp.path().join("outside");
        std::fs::create_dir_all(&external).expect("create external");
        let outside = external.join("issues.jsonl");
        std::fs::write(&outside, "{}").expect("write");

        let result = validate_sync_path(&outside, &beads_dir);
        assert!(
            !result.is_allowed(),
            "external path must be rejected by in-tree validator; got {result:?}"
        );
        assert!(
            matches!(result, PathValidation::OutsideBeadsDir { .. }),
            "expected OutsideBeadsDir, got {result:?}"
        );
    }

    /// PC-1: the explicit-external-jsonl validator
    /// (`validate_sync_path_with_external`) MUST accept a non-`.beads/`
    /// path when `allow_external` is true, but still reject
    /// `.beads/.git/*` and traversal attempts.
    #[test]
    fn validate_sync_path_with_external_accepts_explicit_outside_target() {
        let (temp, beads_dir) = setup_test_beads_dir();
        let external_root = temp.path().join("custom-jsonl-store");
        std::fs::create_dir_all(&external_root).expect("create external root");
        let external_target = external_root.join("my-issues.jsonl");
        std::fs::write(&external_target, "{}").expect("write external");

        let result = validate_sync_path_with_external(&external_target, &beads_dir, true);
        assert!(
            result.is_ok(),
            "explicit external path must be allowed when allow_external=true; got {result:?}"
        );

        // But .git rejection still applies
        let git_under_external = external_root.join(".git").join("HEAD");
        let git_result = validate_sync_path_with_external(&git_under_external, &beads_dir, true);
        assert!(
            git_result.is_err(),
            "explicit external must STILL reject .git/* even with allow_external=true; got {git_result:?}"
        );
    }
}
