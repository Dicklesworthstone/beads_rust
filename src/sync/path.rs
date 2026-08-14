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
//! | `.beads/*.db-wal-cert` | fsqlite parallel-WAL durability certificates |
//! | `.beads/*.db-wal-cert-head` | fsqlite checkpoint hand-off head |
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
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(not(any(unix, windows)))]
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tracing::{debug, warn};

fn raw_os_str_sha256(value: &OsStr) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(value.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in value.encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(value.to_string_lossy().as_bytes());
    crate::util::hex_encode(&hasher.finalize())
}

fn external_path_sha256(path: &Path) -> String {
    raw_os_str_sha256(path.as_os_str())
}

fn external_path_descriptor(path: &Path) -> String {
    format!("<external-path sha256={}>", external_path_sha256(path))
}

/// Files explicitly allowed for sync operations within `.beads/`.
///
/// This list is exhaustive - any file not matching these patterns is rejected.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "db",                 // SQLite database
    "db-wal",             // SQLite WAL
    "db-wal-cert",        // fsqlite 0.2+ parallel-WAL durability certificates
    "db-wal-cert-head",   // fsqlite 0.2+ checkpoint hand-off head
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
    // A dotdot-carrying path that physically resolves inside `.beads/`
    // (e.g. `--db root/x/../.beads/beads.db`) must classify as internal:
    // the raw form never prefix-matches the canonicalized beads_dir, and
    // misclassifying it external refused valid workspaces (#409 routing
    // cluster). Lexical normalization only widens into the *stricter*
    // internal branch, whose validate_sync_path re-normalizes and runs the
    // symlink-escape checks itself.
    let normalized_resolved = normalize_path_lexically(&resolved_path);
    let is_internal = path.starts_with(beads_dir)
        || path.starts_with(&canonical_beads)
        || resolved_path.starts_with(beads_dir)
        || resolved_path.starts_with(&canonical_beads)
        || normalized_resolved.as_deref().is_some_and(|normalized| {
            normalized.starts_with(beads_dir) || normalized.starts_with(&canonical_beads)
        });

    // CRITICAL: Git paths are ALWAYS rejected, even with allow_external. Do
    // not disclose an absolute external path while reporting that rejection.
    let git_check = validate_no_git_path(path);
    if !git_check.is_allowed() {
        let reason = if is_internal {
            git_check
                .rejection_reason()
                .unwrap_or_else(|| "Git path access denied".to_string())
        } else {
            format!(
                "{} targets git internals; sync never accesses .git/",
                external_path_descriptor(path)
            )
        };
        return Err(BeadsError::Config(reason));
    }

    if is_internal {
        return require_valid_sync_path(path, beads_dir);
    }

    // If external paths are allowed, only validate file type (not containment).
    if allow_external {
        let path_sha256 = external_path_sha256(path);
        tracing::info!(
            path = "<external-source>",
            path_sha256,
            "Using external JSONL path (--allow-external-jsonl)"
        );
        return validate_external_jsonl_path(path);
    }

    Err(BeadsError::Config(format!(
        "{} is outside .beads; pass --allow-external-jsonl to authorize it",
        external_path_descriptor(path)
    )))
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
            "{} must be a .jsonl file",
            external_path_descriptor(path)
        )));
    }

    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(BeadsError::Config(format!(
                "{} contains traversal sequences",
                external_path_descriptor(path)
            )));
        }
    }

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(BeadsError::Config(format!(
                "{} must not be a symlink",
                external_path_descriptor(path)
            )));
        }
        if !metadata.is_file() {
            return Err(BeadsError::Config(format!(
                "{} must be a regular file",
                external_path_descriptor(path)
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

    let path_sha256 = external_path_sha256(path);
    if !allow_external {
        let reason = format!(
            "Refusing to {operation} outside .beads: {}",
            external_path_descriptor(path)
        );
        warn!(
            path = "<external-path>",
            path_sha256,
            operation,
            reason = %reason,
            "Sync destructive path rejected"
        );
        return Err(BeadsError::Config(reason));
    }

    match validate_sync_path_with_external(path, beads_dir, true) {
        Ok(()) => {
            debug!(
                path = "<external-path>",
                path_sha256, operation, "External sync path approved for destructive operation"
            );
            Ok(())
        }
        Err(err) => {
            warn!(
                path = "<external-path>",
                path_sha256,
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
    let canonical_beads =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());
    let temp_is_external =
        !temp_path.starts_with(beads_dir) && !temp_path.starts_with(&canonical_beads);
    let safe_temp = if temp_is_external {
        external_path_descriptor(temp_path)
    } else {
        temp_path.display().to_string()
    };
    let target_is_external =
        !target_path.starts_with(beads_dir) && !target_path.starts_with(&canonical_beads);
    let safe_target = if target_is_external {
        external_path_descriptor(target_path)
    } else {
        target_path.display().to_string()
    };

    // Git check is always enforced
    let git_check = validate_no_git_path(temp_path);
    if !git_check.is_allowed() {
        let reason = if temp_is_external {
            format!("{safe_temp} targets git internals; sync never accesses .git/")
        } else {
            git_check
                .rejection_reason()
                .unwrap_or_else(|| "Git path access denied".to_string())
        };
        return Err(BeadsError::Config(reason));
    }

    // Verify temp file is in the same directory as target (PC-4)
    let temp_parent = temp_path.parent();
    let target_parent = target_path.parent();

    if temp_parent != target_parent {
        return Err(BeadsError::Config(format!(
            "Temp file '{}' must be in the same directory as target '{}' (safety invariant PC-4)",
            safe_temp, safe_target
        )));
    }

    let has_tmp_extension = temp_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"));
    if !has_tmp_extension {
        return Err(BeadsError::Config(format!(
            "Temp file '{}' must use a .tmp extension",
            safe_temp
        )));
    }

    validate_sync_path_with_external(temp_path, beads_dir, allow_external)
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonlFileIdentity {
    device_id: u64,
    inode: u64,
}

#[cfg(any(unix, windows))]
impl JsonlFileIdentity {
    /// Returns the filesystem device containing the opened file.
    #[must_use]
    pub const fn device_id(self) -> u64 {
        self.device_id
    }

    /// Returns the inode number of the opened file.
    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }

    /// Returns the Windows volume serial number containing the opened file.
    #[cfg(windows)]
    #[must_use]
    pub const fn volume_serial_number(self) -> u64 {
        self.device_id
    }

    /// Returns the stable Windows file index observed from the opened handle.
    #[cfg(windows)]
    #[must_use]
    pub const fn file_index(self) -> u64 {
        self.inode
    }
}

/// A retained capability for one securely traversed JSONL parent directory.
///
/// Publication code can derive sibling names from this handle without
/// re-resolving the parent through the process working directory.
#[cfg(any(unix, windows))]
#[derive(Debug)]
pub(crate) struct PinnedJsonlParent {
    directory: File,
    canonical_path: PathBuf,
    identity: JsonlFileIdentity,
}

/// Non-Unix builds retain the type-level API but fail closed before a pinned
/// filesystem capability can be constructed.
#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
pub(crate) struct PinnedJsonlParent {
    canonical_path: PathBuf,
}

/// One exact, single-component name interpreted relative to a pinned JSONL
/// parent directory.
#[derive(Debug, Clone)]
pub(crate) struct PinnedJsonlName {
    parent: std::sync::Arc<PinnedJsonlParent>,
    leaf: std::ffi::OsString,
    display_path: PathBuf,
}

fn validate_pinned_jsonl_leaf(leaf: &OsStr) -> Result<()> {
    let leaf_digest = raw_os_str_sha256(leaf);
    let mut components = Path::new(leaf).components();
    let is_exact_normal_component = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(component)), None) if component == leaf
    );
    if !is_exact_normal_component {
        return Err(BeadsError::Config(format!(
            "JSONL leaf <leaf sha256={leaf_digest}> must be exactly one normal filesystem component"
        )));
    }

    #[cfg(unix)]
    let contains_nul = {
        use std::os::unix::ffi::OsStrExt;
        leaf.as_bytes().contains(&0)
    };
    #[cfg(windows)]
    let contains_nul = {
        use std::os::windows::ffi::OsStrExt;
        leaf.encode_wide().any(|unit| unit == 0)
    };
    #[cfg(not(any(unix, windows)))]
    let contains_nul = leaf.to_string_lossy().contains('\0');
    if contains_nul {
        return Err(BeadsError::Config(format!(
            "JSONL leaf <leaf sha256={leaf_digest}> contains an embedded NUL"
        )));
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        if leaf.encode_wide().any(|unit| unit == u16::from(b':')) {
            return Err(BeadsError::Config(format!(
                "JSONL leaf <leaf sha256={leaf_digest}> contains a Windows alternate-data-stream separator"
            )));
        }
    }

    Ok(())
}

impl PinnedJsonlName {
    /// Returns the retained parent-directory capability.
    #[must_use]
    pub(crate) fn parent(&self) -> &PinnedJsonlParent {
        &self.parent
    }

    /// Returns the exact, non-lossy leaf interpreted relative to `parent()`.
    #[must_use]
    pub(crate) fn leaf(&self) -> &OsStr {
        &self.leaf
    }

    /// Returns the diagnostic path captured when this name was constructed.
    ///
    /// Filesystem operations must use `parent()` and `leaf()`, not this path.
    #[must_use]
    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    /// Returns a digest of the raw platform representation of the leaf.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn leaf_sha256(&self) -> String {
        raw_os_str_sha256(&self.leaf)
    }

    /// Derives another exact sibling name under the same retained parent.
    pub(crate) fn with_leaf(&self, leaf: &OsStr) -> Result<Self> {
        validate_pinned_jsonl_leaf(leaf)?;
        Ok(Self {
            parent: std::sync::Arc::clone(&self.parent),
            leaf: leaf.to_os_string(),
            display_path: self.parent.canonical_path().join(leaf),
        })
    }

    /// Resolves one sibling path against the retained parent without
    /// re-traversing that parent through the process namespace.
    pub(crate) fn with_sibling_path(&self, path: &Path) -> Result<Self> {
        #[cfg(any(unix, windows))]
        let absolute = absolute_jsonl_source_path(path)?;
        #[cfg(not(any(unix, windows)))]
        let absolute = path.to_path_buf();
        let parent = absolute.parent().ok_or_else(|| {
            BeadsError::Config(format!(
                "JSONL sibling {} has no parent directory",
                external_path_descriptor(path)
            ))
        })?;
        if parent != self.parent.canonical_path() {
            return Err(BeadsError::SyncConflict {
                message:
                    "JSONL sibling path does not belong to the retained parent-directory capability"
                        .to_string(),
            });
        }
        let leaf = absolute.file_name().ok_or_else(|| {
            BeadsError::Config(format!(
                "JSONL sibling {} has no leaf name",
                external_path_descriptor(path)
            ))
        })?;
        self.with_leaf(leaf)
    }
}

#[cfg(unix)]
fn open_jsonl_directory_via_stable_route(
    display_path: &Path,
    absolute_directory: &Path,
) -> Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    use rustix::io::Errno;

    let descriptor = external_path_descriptor(display_path);
    let directory_flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut route = openat(CWD, "/", directory_flags, Mode::empty()).map_err(|error| {
        BeadsError::Config(format!(
            "Could not open the filesystem root while pinning JSONL parent {descriptor}: {error}"
        ))
    })?;
    let mut components = absolute_directory.components();
    if !matches!(components.next(), Some(std::path::Component::RootDir)) {
        return Err(BeadsError::Config(format!(
            "Could not anchor JSONL parent {descriptor} at the filesystem root"
        )));
    }
    for component in components {
        let std::path::Component::Normal(name) = component else {
            return Err(BeadsError::Config(format!(
                "JSONL parent {descriptor} contains an unsupported filesystem route component"
            )));
        };
        route = match openat(&route, name, directory_flags, Mode::empty()) {
            Ok(next) => next,
            Err(error) if error == Errno::LOOP || error == Errno::NOTDIR => {
                return Err(BeadsError::Config(format!(
                    "JSONL parent component for {descriptor} must not be a symlink and must be a directory"
                )));
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not securely traverse JSONL parent {descriptor}: {error}"
                )));
            }
        };
    }

    let directory = File::from(route);
    let metadata = directory.metadata().map_err(|error| {
        BeadsError::Config(format!(
            "Could not inspect pinned JSONL parent {descriptor}: {error}"
        ))
    })?;
    if !metadata.is_dir() {
        return Err(BeadsError::Config(format!(
            "Pinned JSONL parent {descriptor} is not a directory"
        )));
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_jsonl_directory_via_stable_route(
    display_path: &Path,
    absolute_directory: &Path,
) -> Result<File> {
    use cap_primitives::ambient_authority;
    use cap_primitives::fs::{open_ambient_dir, open_dir_nofollow};

    let descriptor = external_path_descriptor(display_path);
    let mut components = absolute_directory.components();
    let Some(std::path::Component::Prefix(prefix)) = components.next() else {
        return Err(BeadsError::Config(format!(
            "Could not anchor JSONL parent {descriptor} at a Windows volume root"
        )));
    };
    let mut volume_root = PathBuf::from(prefix.as_os_str());
    if !matches!(components.next(), Some(std::path::Component::RootDir)) {
        return Err(BeadsError::Config(format!(
            "Could not anchor JSONL parent {descriptor} at a Windows volume root"
        )));
    }
    volume_root.push(std::path::Component::RootDir.as_os_str());

    // cap-primitives opens directory handles without FILE_SHARE_DELETE on
    // Windows. Retaining the final handle therefore prevents its namespace
    // entry from being renamed or deleted underneath capability-relative
    // operations.
    let mut route =
        open_ambient_dir(&volume_root, ambient_authority()).map_err(|error| {
            BeadsError::Config(format!(
                "Could not open the Windows volume root while pinning JSONL parent {descriptor}: {error}"
            ))
        })?;
    for component in components {
        let std::path::Component::Normal(name) = component else {
            return Err(BeadsError::Config(format!(
                "JSONL parent {descriptor} contains an unsupported filesystem route component"
            )));
        };
        route = open_dir_nofollow(&route, Path::new(name)).map_err(|error| {
            BeadsError::Config(format!(
                "JSONL parent component for {descriptor} must be a non-reparse directory: {error}"
            ))
        })?;
    }

    let metadata = route.metadata().map_err(|error| {
        BeadsError::Config(format!(
            "Could not inspect pinned JSONL parent {descriptor}: {error}"
        ))
    })?;
    if !metadata.is_dir() {
        return Err(BeadsError::Config(format!(
            "Pinned JSONL parent {descriptor} is not a directory"
        )));
    }
    Ok(route)
}

#[cfg(unix)]
impl PinnedJsonlParent {
    /// Returns the lexically normalized absolute route used to acquire this fd.
    #[must_use]
    pub(crate) fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Returns the device/inode identity of the retained directory fd.
    #[must_use]
    pub(crate) const fn identity(&self) -> JsonlFileIdentity {
        self.identity
    }

    /// Borrows the retained directory fd for handle-relative syscalls.
    #[must_use]
    pub(crate) const fn as_file(&self) -> &File {
        &self.directory
    }

    /// Reopens the original route securely and checks that it still names the
    /// retained directory.
    pub(crate) fn verify_route(&self) -> Result<()> {
        let reopened =
            open_jsonl_directory_via_stable_route(&self.canonical_path, &self.canonical_path)
                .map_err(|error| BeadsError::SyncConflict {
                    message: format!(
                        "JSONL parent route could not be re-witnessed after its directory capability was pinned: {error}"
                    ),
                })?;
        let observed = jsonl_file_identity(&reopened.metadata().map_err(|error| {
            BeadsError::Config(format!(
                "Could not re-witness pinned JSONL parent {}: {error}",
                external_path_descriptor(&self.canonical_path)
            ))
        })?);
        if observed != self.identity {
            return Err(BeadsError::SyncConflict {
                message: "JSONL parent route changed after its directory capability was pinned"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Makes namespace changes performed through this retained directory fd
    /// durable.
    pub(crate) fn fsync(&self) -> std::io::Result<()> {
        rustix::fs::fsync(&self.directory).map_err(std::io::Error::from)
    }
}

#[cfg(windows)]
impl PinnedJsonlParent {
    /// Returns the lexically normalized absolute route used to acquire this
    /// retained Windows directory handle.
    #[must_use]
    pub(crate) fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Returns the volume/file-index identity of the retained directory.
    #[must_use]
    pub(crate) const fn identity(&self) -> JsonlFileIdentity {
        self.identity
    }

    /// Borrows the retained directory handle for capability-relative calls.
    #[must_use]
    pub(crate) const fn as_file(&self) -> &File {
        &self.directory
    }

    /// Reopens the original no-follow route and checks that it still names the
    /// retained directory handle.
    pub(crate) fn verify_route(&self) -> Result<()> {
        let reopened =
            open_jsonl_directory_via_stable_route(&self.canonical_path, &self.canonical_path)
                .map_err(|error| BeadsError::SyncConflict {
                    message: format!(
                        "JSONL parent route could not be re-witnessed after its Windows directory capability was pinned: {error}"
                    ),
                })?;
        let observed =
            windows_jsonl_file_identity(&reopened, &self.canonical_path).map_err(|error| {
                BeadsError::SyncConflict {
                    message: format!(
                        "Could not re-witness pinned Windows JSONL parent identity: {error}"
                    ),
                }
            })?;
        if observed != self.identity {
            return Err(BeadsError::SyncConflict {
                message:
                    "JSONL parent route changed after its Windows directory capability was pinned"
                        .to_string(),
            });
        }
        Ok(())
    }

    /// Windows has no documented unprivileged equivalent of directory fsync.
    ///
    /// Returning success here would falsely certify namespace durability.
    pub(crate) fn fsync(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Windows cannot certify directory-entry durability: FlushFileBuffers requires a writable file handle and ReplaceFileW write-through is unsupported",
        ))
    }
}

#[cfg(not(any(unix, windows)))]
impl PinnedJsonlParent {
    #[must_use]
    pub(crate) fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub(crate) fn verify_route(&self) -> Result<()> {
        Err(BeadsError::Config(
            "Pinned JSONL parent handles are unavailable on this platform".to_string(),
        ))
    }

    pub(crate) fn fsync(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "pinned JSONL parent handles are unavailable on this platform",
        ))
    }
}

#[cfg(unix)]
impl PinnedJsonlName {
    fn open_relative_regular_once(&self) -> Result<Option<File>> {
        use rustix::fs::{Mode, OFlags, openat};
        use rustix::io::Errno;

        let leaf_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let descriptor = external_path_descriptor(&self.display_path);
        let opened = match openat(self.parent.as_file(), &self.leaf, leaf_flags, Mode::empty()) {
            Ok(opened) => opened,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => {
                return Err(BeadsError::Config(format!(
                    "JSONL leaf for {descriptor} must not be a symlink"
                )));
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not open pinned JSONL leaf {descriptor}: {error}"
                )));
            }
        };
        let file = File::from(opened);
        regular_jsonl_fd_metadata(&file, &self.display_path)?;
        Ok(Some(file))
    }

    fn verify_relative_identity(&self, expected: JsonlFileIdentity) -> Result<()> {
        let Some(observed) = self.open_relative_regular_once()? else {
            return Err(BeadsError::SyncConflict {
                message: "Pinned JSONL leaf disappeared during identity verification".to_string(),
            });
        };
        let observed = jsonl_file_identity(&observed.metadata().map_err(|error| {
            BeadsError::Config(format!(
                "Could not inspect pinned JSONL leaf {}: {error}",
                external_path_descriptor(&self.display_path)
            ))
        })?);
        if observed != expected {
            return Err(BeadsError::SyncConflict {
                message: "Pinned JSONL leaf changed between secure open and identity verification"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Opens an optional regular leaf relative to the retained parent fd.
    ///
    /// The leaf is reopened after the initial fd validation so a replacement
    /// between `openat` and identity verification is detected.
    pub(crate) fn open_optional_regular(&self) -> Result<Option<OpenedJsonlSource>> {
        let Some(file) = self.open_relative_regular_once()? else {
            return Ok(None);
        };
        let identity = jsonl_file_identity(&file.metadata().map_err(|error| {
            BeadsError::Config(format!(
                "Could not inspect pinned JSONL leaf {}: {error}",
                external_path_descriptor(&self.display_path)
            ))
        })?);
        self.verify_relative_identity(identity)?;
        Ok(Some(OpenedJsonlSource { file, identity }))
    }

    /// Creates a new read/write regular file relative to the retained parent
    /// fd, requesting owner-only `0600` permissions.
    ///
    /// `Ok(None)` means that the exact sibling name already exists. Callers
    /// with a bounded allocator may then try their next prevalidated leaf.
    pub(crate) fn create_new_regular_if_absent(&self) -> Result<Option<File>> {
        use rustix::fs::{Mode, OFlags, openat};
        use rustix::io::Errno;
        use std::os::unix::fs::PermissionsExt;

        let descriptor = external_path_descriptor(&self.display_path);
        let flags = OFlags::RDWR
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK;
        let opened = openat(
            self.parent.as_file(),
            &self.leaf,
            flags,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| match error {
            Errno::LOOP => BeadsError::Config(format!(
                "Pinned JSONL leaf for {descriptor} must not be a symlink"
            )),
            other => BeadsError::Io(std::io::Error::from(other)),
        });
        let opened = match opened {
            Ok(opened) => opened,
            Err(BeadsError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let file = File::from(opened);
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(BeadsError::Io)?;
        let identity = jsonl_file_identity(&regular_jsonl_fd_metadata(&file, &self.display_path)?);
        self.verify_relative_identity(identity)?;
        Ok(Some(file))
    }

    /// Creates a new regular sibling and fails if its exact name exists.
    #[cfg(test)]
    pub(crate) fn create_new_regular(&self) -> Result<File> {
        self.create_new_regular_if_absent()?.ok_or_else(|| {
            BeadsError::Config(format!(
                "Pinned JSONL leaf already exists: {}",
                external_path_descriptor(&self.display_path)
            ))
        })
    }

    /// Removes only the exact regular-file generation identified by
    /// `expected`.
    ///
    /// This is deliberately narrower than a general cleanup primitive. It is
    /// used only after a successful atomic exchange has moved a verified
    /// displaced JSONL generation to an allocator-owned staging leaf.
    pub(crate) fn remove_regular_if_identity(&self, expected: JsonlFileIdentity) -> Result<()> {
        use rustix::fs::{AtFlags, unlinkat};

        self.parent.verify_route()?;
        let opened = self
            .open_optional_regular()?
            .ok_or_else(|| BeadsError::SyncConflict {
                message: "Verified displaced JSONL recovery leaf disappeared before exact cleanup"
                    .to_string(),
            })?;
        if opened.identity() != expected {
            return Err(BeadsError::SyncConflict {
                message:
                    "Displaced JSONL recovery leaf changed before exact handle-relative cleanup"
                        .to_string(),
            });
        }
        self.verify_relative_identity(expected)?;
        unlinkat(self.parent.as_file(), &self.leaf, AtFlags::empty())
            .map_err(|error| BeadsError::Io(std::io::Error::from(error)))?;
        if self.open_optional_regular()?.is_some() {
            return Err(BeadsError::SyncConflict {
                message:
                    "Displaced JSONL recovery leaf reappeared after exact handle-relative cleanup"
                        .to_string(),
            });
        }
        self.parent.verify_route()?;
        Ok(())
    }

    /// Captures the exact current generation of this pinned leaf, if present.
    pub(crate) fn capture_optional(&self) -> Result<Option<JsonlSourceSnapshot>> {
        self.open_optional_regular()?
            .map(|opened| {
                capture_opened_jsonl_source_snapshot_with_verifier(
                    &self.display_path,
                    opened,
                    None,
                    |identity| self.verify_relative_identity(identity),
                )
            })
            .transpose()
    }

    /// Captures the exact current generation of this pinned leaf.
    pub(crate) fn capture(&self) -> Result<JsonlSourceSnapshot> {
        self.capture_optional()?.ok_or_else(|| {
            BeadsError::Config(format!(
                "Pinned JSONL leaf {} does not exist",
                external_path_descriptor(&self.display_path)
            ))
        })
    }
}

#[cfg(windows)]
impl PinnedJsonlName {
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    fn open_relative_regular_once_with_share_mode(&self, share_mode: u32) -> Result<Option<File>> {
        use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt, open};

        let descriptor = external_path_descriptor(&self.display_path);
        let mut options = OpenOptions::new();
        options.read(true);
        options._cap_fs_ext_follow(FollowSymlinks::No);
        options.share_mode(share_mode);
        let file = match open(self.parent.as_file(), Path::new(&self.leaf), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not open pinned Windows JSONL leaf {descriptor} without following reparse points: {error}"
                )));
            }
        };
        regular_jsonl_fd_metadata(&file, &self.display_path)?;
        Ok(Some(file))
    }

    fn verify_relative_identity_with_share_mode(
        &self,
        expected: JsonlFileIdentity,
        share_mode: u32,
    ) -> Result<()> {
        let Some(observed) = self.open_relative_regular_once_with_share_mode(share_mode)? else {
            return Err(BeadsError::SyncConflict {
                message: "Pinned Windows JSONL leaf disappeared during identity verification"
                    .to_string(),
            });
        };
        let observed = windows_jsonl_file_identity(&observed, &self.display_path)?;
        if observed != expected {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pinned Windows JSONL leaf changed between capability-relative open and identity verification"
                        .to_string(),
            });
        }
        Ok(())
    }

    fn verify_relative_identity(&self, expected: JsonlFileIdentity) -> Result<()> {
        self.verify_relative_identity_with_share_mode(expected, Self::FILE_SHARE_READ)
    }

    /// Opens an optional regular Windows leaf relative to the retained parent.
    ///
    /// The handle shares reads only. Existing writers prevent the open, and
    /// after it succeeds new writers, renames, and deletions remain blocked
    /// until the returned handle is closed.
    pub(crate) fn open_optional_regular(&self) -> Result<Option<OpenedJsonlSource>> {
        let Some(file) = self.open_relative_regular_once_with_share_mode(Self::FILE_SHARE_READ)?
        else {
            return Ok(None);
        };
        let identity = windows_jsonl_file_identity(&file, &self.display_path)?;
        self.verify_relative_identity(identity)?;
        Ok(Some(OpenedJsonlSource { file, identity }))
    }

    /// Creates a new regular Windows sibling relative to the retained parent.
    ///
    /// `CREATE_NEW` provides the exact no-clobber allocation guarantee. The
    /// returned writable handle denies delete sharing, so the allocated name
    /// cannot be replaced while the caller stages and syncs its content.
    pub(crate) fn create_new_regular_if_absent(&self) -> Result<Option<File>> {
        use cap_primitives::fs::{FollowSymlinks, OpenOptions, OpenOptionsExt, open};

        let descriptor = external_path_descriptor(&self.display_path);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        options._cap_fs_ext_follow(FollowSymlinks::No);
        options.share_mode(Self::FILE_SHARE_READ | Self::FILE_SHARE_WRITE);
        let file = match open(self.parent.as_file(), Path::new(&self.leaf), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not create pinned Windows JSONL leaf {descriptor}: {error}"
                )));
            }
        };
        regular_jsonl_fd_metadata(&file, &self.display_path)?;
        let identity = windows_jsonl_file_identity(&file, &self.display_path)?;
        self.verify_relative_identity_with_share_mode(
            identity,
            Self::FILE_SHARE_READ | Self::FILE_SHARE_WRITE,
        )?;
        Ok(Some(file))
    }

    /// Creates a new regular sibling and fails if its exact name exists.
    #[cfg(test)]
    pub(crate) fn create_new_regular(&self) -> Result<File> {
        self.create_new_regular_if_absent()?.ok_or_else(|| {
            BeadsError::Config(format!(
                "Pinned Windows JSONL leaf already exists: {}",
                external_path_descriptor(&self.display_path)
            ))
        })
    }

    /// Atomically publishes this exact regular generation at a missing sibling.
    ///
    /// Windows hard-link creation is an atomic no-replace namespace operation.
    /// A read-only, no-write/no-delete-share handle pins the source generation
    /// across the link and both names are identity-checked afterward. The
    /// source name is intentionally retained: current safe dependencies do not
    /// expose by-handle disposition, so deleting it after closing the pinned
    /// handle would reintroduce a hostile-swap race.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn link_regular_no_replace_to(
        &self,
        destination: &Self,
    ) -> Result<JsonlFileIdentity> {
        use cap_primitives::fs::hard_link;

        if self.parent.identity() != destination.parent.identity() {
            return Err(BeadsError::SyncConflict {
                message:
                    "Windows no-replace JSONL publication names do not share one retained parent capability"
                        .to_string(),
            });
        }

        self.parent.verify_route()?;
        let source = self
            .open_optional_regular()?
            .ok_or_else(|| BeadsError::SyncConflict {
                message:
                    "Pinned Windows staged JSONL generation disappeared before no-replace publication"
                        .to_string(),
            })?;
        let source_identity = source.identity();

        hard_link(
            self.parent.as_file(),
            Path::new(&self.leaf),
            destination.parent.as_file(),
            Path::new(&destination.leaf),
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                BeadsError::SyncConflict {
                    message:
                        "JSONL appeared before atomic Windows no-replace publication; refusing to overwrite it"
                            .to_string(),
                }
            } else {
                BeadsError::Io(error)
            }
        })?;

        let published =
            destination
                .open_optional_regular()?
                .ok_or_else(|| BeadsError::SyncConflict {
                    message:
                        "Atomically linked Windows JSONL generation disappeared before verification"
                            .to_string(),
                })?;
        if published.identity() != source_identity {
            return Err(BeadsError::SyncConflict {
                message:
                    "Windows no-replace JSONL publication did not preserve the staged file identity"
                        .to_string(),
            });
        }
        self.verify_relative_identity(source_identity)?;
        destination.verify_relative_identity(source_identity)?;
        self.parent.verify_route()?;
        Ok(source_identity)
    }

    /// Captures the exact current generation of this pinned leaf, if present.
    pub(crate) fn capture_optional(&self) -> Result<Option<JsonlSourceSnapshot>> {
        self.open_optional_regular()?
            .map(|opened| {
                capture_opened_jsonl_source_snapshot_with_verifier(
                    &self.display_path,
                    opened,
                    None,
                    |identity| self.verify_relative_identity(identity),
                )
            })
            .transpose()
    }

    /// Captures the exact current generation of this pinned leaf.
    pub(crate) fn capture(&self) -> Result<JsonlSourceSnapshot> {
        self.capture_optional()?.ok_or_else(|| {
            BeadsError::Config(format!(
                "Pinned Windows JSONL leaf {} does not exist",
                external_path_descriptor(&self.display_path)
            ))
        })
    }
}

#[cfg(not(any(unix, windows)))]
impl PinnedJsonlName {
    pub(crate) fn create_new_regular_if_absent(&self) -> Result<Option<File>> {
        Err(BeadsError::Config(
            "Pinned JSONL file creation is unavailable on this platform".to_string(),
        ))
    }

    #[cfg(test)]
    pub(crate) fn create_new_regular(&self) -> Result<File> {
        Err(BeadsError::Config(
            "Pinned JSONL file creation is unavailable on this platform".to_string(),
        ))
    }

    pub(crate) fn capture_optional(&self) -> Result<Option<JsonlSourceSnapshot>> {
        Err(BeadsError::Config(
            "Pinned JSONL source capture is unavailable on this platform".to_string(),
        ))
    }

    pub(crate) fn capture(&self) -> Result<JsonlSourceSnapshot> {
        Err(BeadsError::Config(
            "Pinned JSONL source capture is unavailable on this platform".to_string(),
        ))
    }
}

/// Pins the parent directory and exact leaf for a JSONL target.
///
/// Existing leaves must be regular files. Missing leaves are accepted so the
/// returned capability can be used for atomic creation.
#[cfg(unix)]
pub(crate) fn pin_jsonl_target(path: &Path) -> Result<PinnedJsonlName> {
    let absolute_target = absolute_jsonl_source_path(path)?;
    let leaf = absolute_target.file_name().ok_or_else(|| {
        BeadsError::Config(format!(
            "JSONL target {} has no leaf name",
            external_path_descriptor(path)
        ))
    })?;
    validate_pinned_jsonl_leaf(leaf)?;
    let parent_path = absolute_target.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "JSONL target {} has no parent directory",
            external_path_descriptor(path)
        ))
    })?;
    let directory = open_jsonl_directory_via_stable_route(path, parent_path)?;
    let identity = jsonl_file_identity(&directory.metadata().map_err(|error| {
        BeadsError::Config(format!(
            "Could not inspect pinned JSONL parent {}: {error}",
            external_path_descriptor(path)
        ))
    })?);
    let parent = std::sync::Arc::new(PinnedJsonlParent {
        directory,
        canonical_path: parent_path.to_path_buf(),
        identity,
    });
    let pinned = PinnedJsonlName {
        parent,
        leaf: leaf.to_os_string(),
        display_path: absolute_target,
    };
    pinned.parent.verify_route()?;
    let _ = pinned.open_optional_regular()?;
    pinned.parent.verify_route()?;
    Ok(pinned)
}

#[cfg(windows)]
pub(crate) fn pin_jsonl_target(path: &Path) -> Result<PinnedJsonlName> {
    let absolute_target = absolute_jsonl_source_path(path)?;
    let leaf = absolute_target.file_name().ok_or_else(|| {
        BeadsError::Config(format!(
            "JSONL target {} has no leaf name",
            external_path_descriptor(path)
        ))
    })?;
    validate_pinned_jsonl_leaf(leaf)?;
    let parent_path = absolute_target.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "JSONL target {} has no parent directory",
            external_path_descriptor(path)
        ))
    })?;
    let directory = open_jsonl_directory_via_stable_route(path, parent_path)?;
    let identity = windows_jsonl_file_identity(&directory, parent_path)?;
    let parent = std::sync::Arc::new(PinnedJsonlParent {
        directory,
        canonical_path: parent_path.to_path_buf(),
        identity,
    });
    let pinned = PinnedJsonlName {
        parent,
        leaf: leaf.to_os_string(),
        display_path: absolute_target,
    };
    pinned.parent.verify_route()?;
    let _ = pinned.open_optional_regular()?;
    pinned.parent.verify_route()?;
    Ok(pinned)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn pin_jsonl_target(_path: &Path) -> Result<PinnedJsonlName> {
    Err(BeadsError::Config(
        "Pinned JSONL parent handles are unavailable on this platform".to_string(),
    ))
}

/// A securely opened JSONL source and the stable identity observed on its fd.
///
/// Keeping the identity beside the `File` makes it possible for callers to
/// retain an auditable witness for the exact filesystem object they read.
#[cfg(any(unix, windows))]
#[derive(Debug)]
pub struct OpenedJsonlSource {
    file: File,
    identity: JsonlFileIdentity,
}

#[cfg(any(unix, windows))]
impl OpenedJsonlSource {
    /// Borrows the securely opened file.
    #[must_use]
    pub const fn as_file(&self) -> &File {
        &self.file
    }

    /// Returns the stable identity captured from the opened fd.
    #[must_use]
    pub const fn identity(&self) -> JsonlFileIdentity {
        self.identity
    }

    /// Consumes the wrapper and returns the securely opened file.
    #[must_use]
    pub fn into_file(self) -> File {
        self.file
    }
}

fn regular_jsonl_fd_metadata(file: &File, path: &Path) -> Result<std::fs::Metadata> {
    let descriptor = external_path_descriptor(path);
    let metadata = file.metadata().map_err(|err| {
        BeadsError::Config(format!(
            "Failed to read metadata on opened JSONL fd for {descriptor}: {err}"
        ))
    })?;

    if !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Opened fd for {descriptor} is not a regular file (possible TOCTOU swap after path validation)"
        )));
    }

    Ok(metadata)
}

#[cfg(unix)]
fn jsonl_file_identity(metadata: &std::fs::Metadata) -> JsonlFileIdentity {
    use std::os::unix::fs::MetadataExt;

    JsonlFileIdentity {
        device_id: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(windows)]
fn windows_jsonl_file_identity(file: &File, path: &Path) -> Result<JsonlFileIdentity> {
    use cap_primitives::fs::{_WindowsByHandle, Metadata};

    let descriptor = external_path_descriptor(path);
    let metadata = Metadata::from_file(file).map_err(|error| {
        BeadsError::Config(format!(
            "Could not inspect the stable Windows file identity for {descriptor}: {error}"
        ))
    })?;
    let volume_serial_number =
        _WindowsByHandle::volume_serial_number(&metadata).ok_or_else(|| {
            BeadsError::Config(format!(
                "Windows volume serial number is unavailable for {descriptor}"
            ))
        })?;
    let file_index = _WindowsByHandle::file_index(&metadata).ok_or_else(|| {
        BeadsError::Config(format!(
            "Windows file index is unavailable for {descriptor}"
        ))
    })?;
    Ok(JsonlFileIdentity {
        device_id: u64::from(volume_serial_number),
        inode: file_index,
    })
}

#[cfg(any(unix, windows))]
fn absolute_jsonl_source_path(path: &Path) -> Result<PathBuf> {
    let descriptor = external_path_descriptor(path);
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(BeadsError::Config(format!(
            "{descriptor} contains traversal sequences"
        )));
    }

    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                BeadsError::Config(format!(
                    "Could not resolve JSONL source {descriptor} against the current directory: {error}"
                ))
            })?
            .join(path)
    };

    normalize_path_lexically(&anchored).ok_or_else(|| {
        BeadsError::Config(format!(
            "Could not normalize JSONL source {descriptor} without escaping its filesystem root"
        ))
    })
}

#[cfg(unix)]
fn reject_symlinked_jsonl_source_route(absolute_path: &Path, display_path: &Path) -> Result<()> {
    let descriptor = external_path_descriptor(display_path);

    for (depth, component_path) in absolute_path.ancestors().enumerate() {
        let metadata = std::fs::symlink_metadata(component_path).map_err(|error| {
            BeadsError::Config(format!(
                "Could not inspect the filesystem route for JSONL source {descriptor}: {error}"
            ))
        })?;

        if metadata.file_type().is_symlink() {
            let route_part = if depth == 0 {
                "source leaf"
            } else {
                "parent component"
            };
            return Err(BeadsError::Config(format!(
                "JSONL {route_part} for {descriptor} must not be a symlink"
            )));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn verify_jsonl_source_path_identity(
    absolute_path: &Path,
    display_path: &Path,
    fd_identity: JsonlFileIdentity,
) -> Result<()> {
    let descriptor = external_path_descriptor(display_path);
    reject_symlinked_jsonl_source_route(absolute_path, display_path)?;

    let path_metadata = std::fs::symlink_metadata(absolute_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not re-read filesystem identity for JSONL source {descriptor}: {error}"
        ))
    })?;
    if !path_metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Filesystem path for JSONL source {descriptor} is not a regular file"
        )));
    }

    let path_identity = jsonl_file_identity(&path_metadata);
    if path_identity != fd_identity {
        return Err(BeadsError::Config(format!(
            "JSONL source {descriptor} changed between secure open and identity verification"
        )));
    }

    Ok(())
}

#[cfg(unix)]
fn open_jsonl_source_via_stable_route(path: &Path, absolute_path: &Path) -> Result<Option<File>> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    use rustix::io::Errno;

    let descriptor = external_path_descriptor(path);
    let directory_flags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let leaf_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut route = openat(CWD, "/", directory_flags, Mode::empty()).map_err(|error| {
        BeadsError::Config(format!(
            "Could not open the filesystem root while securing JSONL source {descriptor}: {error}"
        ))
    })?;
    let mut components = absolute_path.components().peekable();
    if !matches!(components.next(), Some(std::path::Component::RootDir)) {
        return Err(BeadsError::Config(format!(
            "Could not anchor JSONL source {descriptor} at the filesystem root"
        )));
    }
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(BeadsError::Config(format!(
                "JSONL source {descriptor} contains an unsupported filesystem route component"
            )));
        };
        let is_leaf = components.peek().is_none();
        let flags = if is_leaf { leaf_flags } else { directory_flags };
        route = match openat(&route, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) if is_leaf => return Ok(None),
            Err(error) if is_leaf && error == Errno::LOOP => {
                return Err(BeadsError::Config(format!(
                    "JSONL source leaf for {descriptor} must not be a symlink"
                )));
            }
            Err(error) if !is_leaf && (error == Errno::LOOP || error == Errno::NOTDIR) => {
                return Err(BeadsError::Config(format!(
                    "JSONL parent component for {descriptor} must not be a symlink and must be a directory"
                )));
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not securely traverse JSONL source {descriptor}: {error}"
                )));
            }
        };
    }
    Ok(Some(File::from(route)))
}

#[cfg(unix)]
fn finish_opened_jsonl_source(
    path: &Path,
    absolute_path: &Path,
    file: File,
) -> Result<OpenedJsonlSource> {
    let identity = jsonl_file_identity(&regular_jsonl_fd_metadata(&file, path)?);
    verify_jsonl_source_path_identity(absolute_path, path, identity)?;
    Ok(OpenedJsonlSource { file, identity })
}

#[cfg(unix)]
fn open_jsonl_source_nofollow_impl<F>(path: &Path, after_open: F) -> Result<OpenedJsonlSource>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let descriptor = external_path_descriptor(path);
    let absolute_path = absolute_jsonl_source_path(path)?;
    let file = open_jsonl_source_via_stable_route(path, &absolute_path)?
        .ok_or_else(|| BeadsError::Config(format!("JSONL source {descriptor} does not exist")))?;

    after_open().map_err(|error| {
        BeadsError::Config(format!(
            "Post-open JSONL source verification hook failed for {descriptor}: {error}"
        ))
    })?;
    finish_opened_jsonl_source(path, &absolute_path, file)
}

#[cfg(windows)]
fn open_jsonl_source_nofollow_impl<F>(path: &Path, after_open: F) -> Result<OpenedJsonlSource>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let descriptor = external_path_descriptor(path);
    let pinned = pin_jsonl_target(path)?;
    let opened = pinned
        .open_optional_regular()?
        .ok_or_else(|| BeadsError::Config(format!("JSONL source {descriptor} does not exist")))?;

    after_open().map_err(|error| {
        BeadsError::Config(format!(
            "Post-open Windows JSONL source verification hook failed for {descriptor}: {error}"
        ))
    })?;
    pinned.verify_relative_identity(opened.identity())?;
    pinned.parent.verify_route()?;
    Ok(opened)
}

/// Opens an existing JSONL source without following symlinks.
///
/// This platform capability primitive:
///
/// 1. rejects traversal and opens every route component relative to a retained
///    parent handle without following symlinks or Windows reparse points;
/// 2. opens read-only and retains the exact opened generation;
/// 3. requires the opened fd to identify a regular file; and
/// 4. compares the handle's stable filesystem identity with a fresh
///    capability-relative lookup.
///
/// The returned `File` remains authoritative even if the path is replaced
/// after this function returns.
///
/// # Errors
///
/// Returns `BeadsError::Config` if the path cannot be inspected or securely
/// opened, names a symlink or non-regular file, or changes during the
/// open-and-verify sequence.
#[cfg(any(unix, windows))]
pub fn open_jsonl_source_nofollow(path: &Path) -> Result<OpenedJsonlSource> {
    open_jsonl_source_nofollow_impl(path, || Ok(()))
}

#[cfg(unix)]
fn open_optional_jsonl_source_nofollow(path: &Path) -> Result<Option<OpenedJsonlSource>> {
    let absolute_path = absolute_jsonl_source_path(path)?;
    open_jsonl_source_via_stable_route(path, &absolute_path)?
        .map(|file| finish_opened_jsonl_source(path, &absolute_path, file))
        .transpose()
}

#[cfg(windows)]
fn open_optional_jsonl_source_nofollow(path: &Path) -> Result<Option<OpenedJsonlSource>> {
    pin_jsonl_target(path)?.open_optional_regular()
}

/// Exact immutable content captured from one securely opened JSONL file.
///
/// Every parser, hash, prefix probe, and import phase for a logical operation
/// can open an independent reader on this value instead of reopening a mutable
/// path. Unix and Windows snapshots use a private temporary spool so
/// arbitrarily large or sparse sources do not require a whole-file heap
/// allocation. The snapshot deliberately keeps the exact raw digest separate
/// from higher-level canonical content hashes: whitespace-only changes still
/// matter to overwrite guards.
#[derive(Debug)]
pub(crate) struct JsonlSourceSnapshot {
    display_path: PathBuf,
    #[cfg(any(unix, windows))]
    backing: File,
    #[cfg(not(any(unix, windows)))]
    bytes: Arc<[u8]>,
    raw_sha256: String,
    content_sha256: String,
    modified: SystemTime,
    size: u64,
    #[cfg(any(unix, windows))]
    // The Windows receipt integration consumes this once sync/mod.rs enables
    // native publication; keep the capability witness without a broad allow.
    #[cfg_attr(windows, allow(dead_code))]
    identity: JsonlFileIdentity,
}

impl JsonlSourceSnapshot {
    #[must_use]
    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn reader(&self) -> std::io::BufReader<JsonlSnapshotReader<'_>> {
        std::io::BufReader::new(JsonlSnapshotReader {
            file: &self.backing,
            offset: 0,
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn reader(&self) -> std::io::BufReader<std::io::Cursor<&[u8]>> {
        std::io::BufReader::new(std::io::Cursor::new(self.bytes.as_ref()))
    }

    #[must_use]
    pub(crate) fn raw_sha256(&self) -> &str {
        &self.raw_sha256
    }

    #[must_use]
    pub(crate) fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    #[must_use]
    pub(crate) const fn modified(&self) -> SystemTime {
        self.modified
    }

    #[must_use]
    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    #[cfg(any(unix, windows))]
    // See the field-level note: Windows path capture is implemented before
    // the higher-level publication receipt is wired into sync/mod.rs.
    #[cfg_attr(windows, allow(dead_code))]
    #[must_use]
    pub(crate) const fn identity(&self) -> JsonlFileIdentity {
        self.identity
    }
}

#[cfg(any(unix, windows))]
pub(crate) struct JsonlSnapshotReader<'a> {
    file: &'a File,
    offset: u64,
}

#[cfg(any(unix, windows))]
impl std::io::Read for JsonlSnapshotReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;

        #[cfg(unix)]
        let read = self.file.read_at(buffer, self.offset)?;
        #[cfg(windows)]
        let read = self.file.seek_read(buffer, self.offset)?;
        self.offset = self
            .offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("JSONL snapshot reader offset overflow"))?;
        Ok(read)
    }
}

#[cfg(any(unix, windows))]
fn jsonl_capture_timeout() -> BeadsError {
    BeadsError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "immutable JSONL source capture exceeded its observation deadline",
    ))
}

#[cfg(any(unix, windows))]
fn ensure_jsonl_capture_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(jsonl_capture_timeout());
    }
    Ok(())
}

#[cfg(any(unix, windows))]
struct DeadlineReader<R> {
    inner: R,
    deadline: Option<Instant>,
}

#[cfg(any(unix, windows))]
impl<R: std::io::Read> std::io::Read for DeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "immutable JSONL source capture exceeded its observation deadline",
            ));
        }
        let read = self.inner.read(buffer)?;
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "immutable JSONL source capture exceeded its observation deadline",
            ));
        }
        Ok(read)
    }
}

#[cfg(any(unix, windows))]
fn compute_snapshot_content_sha256(backing: &File, deadline: Option<Instant>) -> Result<String> {
    use std::io::BufRead;

    ensure_jsonl_capture_deadline(deadline)?;
    let mut reader = std::io::BufReader::new(DeadlineReader {
        inner: JsonlSnapshotReader {
            file: backing,
            offset: 0,
        },
        deadline,
    });
    let mut hasher = Sha256::new();
    let mut line = Vec::with_capacity(4096);
    loop {
        ensure_jsonl_capture_deadline(deadline)?;
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_ascii();
        if !trimmed.is_empty() {
            hasher.update(trimmed);
            hasher.update(b"\n");
        }
    }
    ensure_jsonl_capture_deadline(deadline)?;
    Ok(crate::util::hex_encode(&hasher.finalize()))
}

#[cfg(unix)]
fn jsonl_fd_stability_witness(metadata: &std::fs::Metadata) -> Result<(u64, SystemTime, i64, i64)> {
    use std::os::unix::fs::MetadataExt;

    Ok((
        metadata.len(),
        metadata.modified()?,
        metadata.ctime(),
        metadata.ctime_nsec(),
    ))
}

#[cfg(windows)]
fn jsonl_fd_stability_witness(metadata: &std::fs::Metadata) -> Result<(u64, SystemTime, u64, u64)> {
    use std::os::windows::fs::MetadataExt;

    Ok((
        metadata.file_size(),
        metadata.modified()?,
        metadata.last_write_time(),
        metadata.creation_time(),
    ))
}

/// Captures one exact, stable JSONL source generation without following
/// symlinks.
///
/// The secure fd is opened once, copied through a fixed-size buffer into a
/// private anonymous backing file, and checked for ordinary in-place mutation
/// before the path-to-fd identity is checked again. Callers must perform every
/// semantic pass through `reader()` rather than reopening `display_path()`.
///
/// # Errors
///
/// Returns a deterministic configuration or synchronization error when the
/// source is unsafe, changes during capture, cannot be represented in memory,
/// or cannot be read completely.
#[cfg(any(unix, windows))]
fn capture_opened_jsonl_source_snapshot_with_verifier<VerifyIdentity>(
    path: &Path,
    opened: OpenedJsonlSource,
    deadline: Option<Instant>,
    verify_identity: VerifyIdentity,
) -> Result<JsonlSourceSnapshot>
where
    VerifyIdentity: FnOnce(JsonlFileIdentity) -> Result<()>,
{
    use std::io::{Read, Write};

    ensure_jsonl_capture_deadline(deadline)?;
    let identity = opened.identity();
    let before_metadata = regular_jsonl_fd_metadata(opened.as_file(), path)?;
    let before_witness = jsonl_fd_stability_witness(&before_metadata)?;
    ensure_jsonl_capture_deadline(deadline)?;
    let mut backing = tempfile::tempfile().map_err(|error| {
        BeadsError::Config(format!(
            "Could not create private backing for JSONL source {}: {error}",
            external_path_descriptor(path)
        ))
    })?;
    ensure_jsonl_capture_deadline(deadline)?;
    let mut file = opened.into_file();
    let mut hasher = Sha256::new();
    let mut remaining = before_metadata.len();
    let mut buffer = vec![0_u8; 64 * 1024];
    while remaining > 0 {
        ensure_jsonl_capture_deadline(deadline)?;
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded snapshot read size fits usize");
        let read = file.read(&mut buffer[..wanted])?;
        ensure_jsonl_capture_deadline(deadline)?;
        if read == 0 {
            return Err(BeadsError::SyncConflict {
                message:
                    "JSONL source became shorter while its immutable snapshot was being captured"
                        .to_string(),
            });
        }
        backing.write_all(&buffer[..read])?;
        ensure_jsonl_capture_deadline(deadline)?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    ensure_jsonl_capture_deadline(deadline)?;
    let mut eof_probe = [0_u8; 1];
    if file.read(&mut eof_probe)? != 0 {
        return Err(BeadsError::SyncConflict {
            message: "JSONL source grew while its immutable snapshot was being captured"
                .to_string(),
        });
    }
    ensure_jsonl_capture_deadline(deadline)?;
    let after_metadata = regular_jsonl_fd_metadata(&file, path)?;
    let after_witness = jsonl_fd_stability_witness(&after_metadata)?;
    if before_witness != after_witness {
        return Err(BeadsError::SyncConflict {
            message: "JSONL source changed while its immutable snapshot was being captured"
                .to_string(),
        });
    }

    ensure_jsonl_capture_deadline(deadline)?;
    verify_identity(identity)?;
    ensure_jsonl_capture_deadline(deadline)?;
    let content_sha256 = compute_snapshot_content_sha256(&backing, deadline)?;
    ensure_jsonl_capture_deadline(deadline)?;

    Ok(JsonlSourceSnapshot {
        display_path: path.to_path_buf(),
        backing,
        raw_sha256: crate::util::hex_encode(&hasher.finalize()),
        content_sha256,
        modified: before_witness.1,
        size: before_witness.0,
        identity,
    })
}

#[cfg(unix)]
fn capture_opened_jsonl_source_snapshot(
    path: &Path,
    opened: OpenedJsonlSource,
) -> Result<JsonlSourceSnapshot> {
    let absolute_path = absolute_jsonl_source_path(path)?;
    capture_opened_jsonl_source_snapshot_with_verifier(path, opened, None, |identity| {
        verify_jsonl_source_path_identity(&absolute_path, path, identity)
    })
}

#[cfg(windows)]
fn capture_opened_jsonl_source_snapshot(
    path: &Path,
    opened: OpenedJsonlSource,
) -> Result<JsonlSourceSnapshot> {
    let pinned = pin_jsonl_target(path)?;
    capture_opened_jsonl_source_snapshot_with_verifier(path, opened, None, |identity| {
        pinned.verify_relative_identity(identity)
    })
}

#[cfg(unix)]
fn capture_opened_jsonl_source_snapshot_until(
    path: &Path,
    opened: OpenedJsonlSource,
    deadline: Instant,
) -> Result<JsonlSourceSnapshot> {
    ensure_jsonl_capture_deadline(Some(deadline))?;
    let absolute_path = absolute_jsonl_source_path(path)?;
    ensure_jsonl_capture_deadline(Some(deadline))?;
    capture_opened_jsonl_source_snapshot_with_verifier(path, opened, Some(deadline), |identity| {
        verify_jsonl_source_path_identity(&absolute_path, path, identity)
    })
}

#[cfg(windows)]
fn capture_opened_jsonl_source_snapshot_until(
    path: &Path,
    opened: OpenedJsonlSource,
    deadline: Instant,
) -> Result<JsonlSourceSnapshot> {
    ensure_jsonl_capture_deadline(Some(deadline))?;
    let pinned = pin_jsonl_target(path)?;
    ensure_jsonl_capture_deadline(Some(deadline))?;
    capture_opened_jsonl_source_snapshot_with_verifier(path, opened, Some(deadline), |identity| {
        pinned.verify_relative_identity(identity)
    })
}

#[cfg(any(unix, windows))]
pub(crate) fn capture_jsonl_source_snapshot(path: &Path) -> Result<JsonlSourceSnapshot> {
    let opened = open_jsonl_source_nofollow(path)?;
    capture_opened_jsonl_source_snapshot(path, opened)
}

#[cfg(any(unix, windows))]
pub(crate) fn capture_optional_jsonl_source_snapshot(
    path: &Path,
) -> Result<Option<JsonlSourceSnapshot>> {
    open_optional_jsonl_source_nofollow(path)?
        .map(|opened| capture_opened_jsonl_source_snapshot(path, opened))
        .transpose()
}

/// Captures an optional immutable JSONL generation while cooperatively
/// enforcing an absolute observation deadline.
///
/// Regular-file reads and writes cannot be preempted portably once the kernel
/// has accepted them, so one individual filesystem call may finish after the
/// deadline. The capture checks the deadline before and after every bounded
/// chunk, throughout both hashes, and around identity verification; an
/// over-budget result is never returned as a successful snapshot.
#[cfg(any(unix, windows))]
pub(crate) fn capture_optional_jsonl_source_snapshot_until(
    path: &Path,
    deadline: Instant,
) -> Result<Option<JsonlSourceSnapshot>> {
    ensure_jsonl_capture_deadline(Some(deadline))?;
    let opened = open_optional_jsonl_source_nofollow(path)?;
    ensure_jsonl_capture_deadline(Some(deadline))?;
    opened
        .map(|opened| capture_opened_jsonl_source_snapshot_until(path, opened, deadline))
        .transpose()
}

/// Other native builds fail closed until they have an equivalent
/// reparse-point-resistant stable-handle implementation.
#[cfg(not(any(unix, windows)))]
pub(crate) fn capture_jsonl_source_snapshot(_path: &Path) -> Result<JsonlSourceSnapshot> {
    Err(BeadsError::Config(
        "Immutable JSONL source capture is unavailable on this platform; refusing to read or mutate SQLite without stable file identity"
            .to_string(),
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn capture_optional_jsonl_source_snapshot(
    _path: &Path,
) -> Result<Option<JsonlSourceSnapshot>> {
    Err(BeadsError::Config(
        "Immutable JSONL source capture is unavailable on this platform; refusing to read or mutate SQLite without stable file identity"
            .to_string(),
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn capture_optional_jsonl_source_snapshot_until(
    _path: &Path,
    _deadline: Instant,
) -> Result<Option<JsonlSourceSnapshot>> {
    Err(BeadsError::Config(
        "Immutable JSONL source capture is unavailable on this platform; refusing to read or mutate SQLite without stable file identity"
            .to_string(),
    ))
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
    regular_jsonl_fd_metadata(file, path).map(|_| ())
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

    #[cfg(any(unix, windows))]
    #[test]
    fn deadline_aware_snapshot_matches_the_unbounded_capture() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, b"  {\"id\":\"br-a\"}  \n\n{\"id\":\"br-b\"}\n")
            .expect("write JSONL fixture");

        let ordinary = capture_optional_jsonl_source_snapshot(&path)
            .expect("capture ordinary snapshot")
            .expect("ordinary source should be present");
        let bounded = capture_optional_jsonl_source_snapshot_until(
            &path,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .expect("capture deadline-aware snapshot")
        .expect("deadline-aware source should be present");

        assert_eq!(bounded.size(), ordinary.size());
        assert_eq!(bounded.raw_sha256(), ordinary.raw_sha256());
        assert_eq!(bounded.content_sha256(), ordinary.content_sha256());
        assert_eq!(bounded.identity(), ordinary.identity());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn deadline_aware_snapshot_refuses_expired_and_overrun_reads() {
        use std::io::Read;

        struct SlowReader;
        impl Read for SlowReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_millis(5));
                buffer[0] = b'x';
                Ok(1)
            }
        }

        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, b"{\"id\":\"br-timeout\"}\n").expect("write JSONL fixture");

        let expired = capture_optional_jsonl_source_snapshot_until(&path, Instant::now())
            .expect_err("an expired observation deadline must fail");
        assert!(
            matches!(
                expired,
                BeadsError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut
            ),
            "unexpected expired-deadline error: {expired}"
        );

        let mut reader = DeadlineReader {
            inner: SlowReader,
            deadline: Some(Instant::now() + std::time::Duration::from_millis(1)),
        };
        let error = reader
            .read(&mut [0_u8; 1])
            .expect_err("a read that crosses the deadline must not be accepted");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
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

    #[cfg(unix)]
    #[test]
    fn open_jsonl_source_nofollow_accepts_regular_file() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        std::fs::write(&path, "{\"id\":\"br-test\"}\n").expect("write JSONL source");

        let opened = open_jsonl_source_nofollow(&path).expect("securely open regular JSONL");
        let metadata = opened
            .as_file()
            .metadata()
            .expect("read opened fd metadata");
        assert_eq!(opened.identity().device_id(), metadata.dev());
        assert_eq!(opened.identity().inode(), metadata.ino());

        let mut contents = String::new();
        opened
            .into_file()
            .read_to_string(&mut contents)
            .expect("read securely opened JSONL");
        assert_eq!(contents, "{\"id\":\"br-test\"}\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_jsonl_source_nofollow_rejects_directory() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("directory.jsonl");
        std::fs::create_dir(&path).expect("create directory");

        let error =
            open_jsonl_source_nofollow(&path).expect_err("directory must not open as JSONL");
        assert!(
            error.to_string().contains("not a regular file"),
            "error should identify the regular-file requirement: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_jsonl_source_nofollow_rejects_leaf_symlink_without_mutating_target() {
        use std::os::unix::fs::symlink;

        let (temp, beads_dir) = setup_test_beads_dir();
        let target = temp.path().join("target.jsonl");
        let target_contents = b"{\"protected\":true}\n";
        std::fs::write(&target, target_contents).expect("write symlink target");
        let path = beads_dir.join("issues.jsonl");
        symlink(&target, &path).expect("create leaf symlink");

        let error = open_jsonl_source_nofollow(&path).expect_err("leaf symlink must be rejected");
        assert!(
            error.to_string().contains("source leaf")
                && error.to_string().contains("must not be a symlink"),
            "error should identify the leaf symlink: {error}"
        );
        assert_eq!(
            std::fs::read(&target).expect("read symlink target after rejection"),
            target_contents,
            "rejecting a source symlink must not mutate its target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_jsonl_source_nofollow_rejects_parent_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (temp, beads_dir) = setup_test_beads_dir();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).expect("create outside directory");
        std::fs::write(outside.join("issues.jsonl"), "{}\n").expect("write outside JSONL");

        let linked_parent = beads_dir.join("linked");
        symlink(&outside, &linked_parent).expect("create escaping parent symlink");
        let path = linked_parent.join("issues.jsonl");

        let error = open_jsonl_source_nofollow(&path)
            .expect_err("source below a symlinked parent must be rejected");
        assert!(
            error.to_string().contains("parent component")
                && error.to_string().contains("must not be a symlink"),
            "error should identify the parent symlink: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_jsonl_source_nofollow_rejects_path_replacement_before_identity_recheck() {
        let (_temp, beads_dir) = setup_test_beads_dir();
        let path = beads_dir.join("issues.jsonl");
        let replacement = beads_dir.join("replacement.jsonl");
        let displaced = beads_dir.join("displaced.jsonl");
        std::fs::write(&path, "{\"source\":\"original\"}\n").expect("write original source");
        std::fs::write(&replacement, "{\"source\":\"replacement\"}\n")
            .expect("write replacement source");

        let error = open_jsonl_source_nofollow_impl(&path, || {
            std::fs::rename(&path, &displaced)?;
            std::fs::rename(&replacement, &path)
        })
        .expect_err("path replacement must fail identity verification");

        assert!(
            error
                .to_string()
                .contains("changed between secure open and identity verification"),
            "error should identify the fd/path identity mismatch: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&displaced).expect("read displaced original"),
            "{\"source\":\"original\"}\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read installed replacement"),
            "{\"source\":\"replacement\"}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pin_jsonl_target_rejects_symlinked_and_non_directory_parents() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp directory");
        let outside = temp.path().join("outside");
        let linked_parent = temp.path().join("linked-parent");
        let non_directory_parent = temp.path().join("not-a-directory");
        std::fs::create_dir(&outside).expect("create outside directory");
        symlink(&outside, &linked_parent).expect("create parent symlink");
        std::fs::write(&non_directory_parent, b"not a directory")
            .expect("write non-directory parent");

        let symlink_error = pin_jsonl_target(&linked_parent.join("issues.jsonl"))
            .expect_err("symlinked parent must be rejected");
        assert!(
            symlink_error.to_string().contains("parent component")
                && symlink_error.to_string().contains("must not be a symlink"),
            "unexpected symlinked-parent error: {symlink_error}"
        );

        let non_directory_error = pin_jsonl_target(&non_directory_parent.join("issues.jsonl"))
            .expect_err("non-directory parent must be rejected");
        assert!(
            non_directory_error
                .to_string()
                .contains("must be a directory"),
            "unexpected non-directory-parent error: {non_directory_error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pin_jsonl_target_rejects_symlinked_and_nonregular_leaves() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp directory");
        let parent = temp.path().join("parent");
        let outside = temp.path().join("outside.jsonl");
        let symlink_leaf = parent.join("linked.jsonl");
        let directory_leaf = parent.join("directory.jsonl");
        std::fs::create_dir(&parent).expect("create parent directory");
        std::fs::write(&outside, b"{\"outside\":true}\n").expect("write outside target");
        symlink(&outside, &symlink_leaf).expect("create leaf symlink");
        std::fs::create_dir(&directory_leaf).expect("create directory leaf");

        let symlink_error =
            pin_jsonl_target(&symlink_leaf).expect_err("symlinked leaf must be rejected");
        assert!(
            symlink_error.to_string().contains("leaf")
                && symlink_error.to_string().contains("must not be a symlink"),
            "unexpected symlinked-leaf error: {symlink_error}"
        );
        assert_eq!(
            std::fs::read(&outside).expect("read outside target"),
            b"{\"outside\":true}\n"
        );

        let directory_error =
            pin_jsonl_target(&directory_leaf).expect_err("directory leaf must be rejected");
        assert!(
            directory_error.to_string().contains("not a regular file"),
            "unexpected directory-leaf error: {directory_error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_jsonl_name_rejects_non_leaf_components_and_nul() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().expect("create temp directory");
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).expect("create parent directory");
        let pinned =
            pin_jsonl_target(&parent.join("issues.jsonl")).expect("pin missing JSONL target");

        for invalid in ["", ".", "..", "nested/name", "/absolute"] {
            let error = pinned
                .with_leaf(OsStr::new(invalid))
                .expect_err("non-leaf component must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("one normal filesystem component"),
                "unexpected invalid-leaf error for {invalid:?}: {error}"
            );
        }

        let nul_name = OsString::from_vec(b"nul\0name.jsonl".to_vec());
        let error = pinned
            .with_leaf(&nul_name)
            .expect_err("embedded NUL must be rejected");
        assert!(
            error.to_string().contains("embedded NUL"),
            "unexpected embedded-NUL error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_jsonl_parent_detects_route_replacement() {
        let temp = TempDir::new().expect("create temp directory");
        let routed_parent = temp.path().join("live");
        let displaced_parent = temp.path().join("displaced");
        std::fs::create_dir(&routed_parent).expect("create routed parent");
        let pinned = pin_jsonl_target(&routed_parent.join("issues.jsonl"))
            .expect("pin missing JSONL target");

        std::fs::rename(&routed_parent, &displaced_parent).expect("displace pinned parent");
        std::fs::create_dir(&routed_parent).expect("create replacement parent");

        let error = pinned
            .parent()
            .verify_route()
            .expect_err("replacement route must not match pinned parent");
        assert!(
            matches!(error, BeadsError::SyncConflict { .. }),
            "route replacement should be a synchronization conflict: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_jsonl_parent_reports_disappeared_or_symlinked_route_as_conflict() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp directory");
        let routed_parent = temp.path().join("live");
        let displaced_parent = temp.path().join("displaced");
        std::fs::create_dir(&routed_parent).expect("create routed parent");
        let pinned = pin_jsonl_target(&routed_parent.join("issues.jsonl"))
            .expect("pin missing JSONL target");

        std::fs::rename(&routed_parent, &displaced_parent).expect("displace pinned parent");
        let missing_error = pinned
            .parent()
            .verify_route()
            .expect_err("missing route must be a conflict");
        assert!(
            matches!(missing_error, BeadsError::SyncConflict { .. }),
            "missing route should be a synchronization conflict: {missing_error}"
        );

        symlink(&displaced_parent, &routed_parent).expect("replace route with symlink");
        let symlink_error = pinned
            .parent()
            .verify_route()
            .expect_err("symlinked replacement route must be a conflict");
        assert!(
            matches!(symlink_error, BeadsError::SyncConflict { .. }),
            "symlinked route should be a synchronization conflict: {symlink_error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_parent_traversal_handles_root_but_root_is_not_a_target_leaf() {
        let root = open_jsonl_directory_via_stable_route(Path::new("/"), Path::new("/"))
            .expect("pin filesystem root");
        assert!(
            root.metadata()
                .expect("inspect pinned filesystem root")
                .is_dir()
        );

        let error =
            pin_jsonl_target(Path::new("/")).expect_err("filesystem root has no target leaf");
        assert!(
            error.to_string().contains("has no leaf name"),
            "unexpected root-target error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_jsonl_operations_remain_on_retained_parent_after_route_swap() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("create temp directory");
        let routed_parent = temp.path().join("live");
        let displaced_parent = temp.path().join("displaced");
        let target_path = routed_parent.join("issues.jsonl");
        std::fs::create_dir(&routed_parent).expect("create routed parent");
        std::fs::write(&target_path, b"{\"source\":\"pinned\"}\n")
            .expect("write pinned generation");
        let pinned = pin_jsonl_target(&target_path).expect("pin existing JSONL target");
        let sibling = pinned
            .with_leaf(OsStr::new("sibling.jsonl"))
            .expect("derive pinned sibling");

        std::fs::rename(&routed_parent, &displaced_parent).expect("displace pinned parent");
        std::fs::create_dir(&routed_parent).expect("create replacement route");
        std::fs::write(&target_path, b"{\"source\":\"replacement\"}\n")
            .expect("write replacement-route generation");

        let mut opened = pinned
            .open_optional_regular()
            .expect("open through retained parent")
            .expect("pinned target remains present")
            .into_file();
        let mut opened_contents = String::new();
        opened
            .read_to_string(&mut opened_contents)
            .expect("read pinned target");
        assert_eq!(opened_contents, "{\"source\":\"pinned\"}\n");

        let snapshot = pinned.capture().expect("capture through retained parent");
        let mut captured_contents = String::new();
        snapshot
            .reader()
            .read_to_string(&mut captured_contents)
            .expect("read pinned snapshot");
        assert_eq!(captured_contents, "{\"source\":\"pinned\"}\n");

        drop(pinned);
        let mut sibling_file = sibling
            .create_new_regular()
            .expect("derived sibling must retain the pinned parent fd");
        sibling_file
            .write_all(b"{\"sibling\":\"pinned\"}\n")
            .expect("write pinned sibling");
        sibling_file.sync_all().expect("sync pinned sibling");
        assert_eq!(
            sibling_file
                .metadata()
                .expect("inspect pinned sibling")
                .permissions()
                .mode()
                & 0o077,
            0,
            "handle-relative creation must not grant group or other permissions"
        );
        sibling.parent().fsync().expect("sync pinned parent");
        assert_eq!(
            std::fs::read(displaced_parent.join("sibling.jsonl"))
                .expect("read sibling in retained parent"),
            b"{\"sibling\":\"pinned\"}\n"
        );
        assert!(
            !routed_parent.join("sibling.jsonl").exists(),
            "handle-relative creation must not reach the replacement route"
        );
        assert_eq!(
            std::fs::read(&target_path).expect("read replacement-route target"),
            b"{\"source\":\"replacement\"}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_hashes_distinguish_invalid_utf8_sibling_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().expect("create temp directory");
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).expect("create parent directory");
        let pinned =
            pin_jsonl_target(&parent.join("issues.jsonl")).expect("pin missing JSONL target");
        let first_leaf = OsString::from_vec(b"sibling-\x80.jsonl".to_vec());
        let second_leaf = OsString::from_vec(b"sibling-\x81.jsonl".to_vec());
        let first = pinned
            .with_leaf(&first_leaf)
            .expect("derive first invalid-UTF8 sibling");
        let second = pinned
            .with_leaf(&second_leaf)
            .expect("derive second invalid-UTF8 sibling");

        assert_ne!(first.leaf(), second.leaf());
        assert_ne!(first.display_path(), second.display_path());
        assert_ne!(first.leaf_sha256(), second.leaf_sha256());
        assert_ne!(
            external_path_sha256(first.display_path()),
            external_path_sha256(second.display_path())
        );
    }

    #[cfg(not(any(unix, windows)))]
    #[test]
    fn pinned_jsonl_target_fails_closed_without_native_handles() {
        let error = pin_jsonl_target(Path::new("issues.jsonl"))
            .expect_err("unsupported native pinning must fail closed");
        assert!(
            error
                .to_string()
                .contains("Pinned JSONL parent handles are unavailable"),
            "unexpected unsupported-platform pinning error: {error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_jsonl_open_capture_and_identity_are_handle_stable() {
        use std::io::Read;

        let temp = TempDir::new().expect("create Windows temp directory");
        let parent = temp.path().join("parent");
        let target = parent.join("issues.jsonl");
        std::fs::create_dir(&parent).expect("create Windows JSONL parent");
        std::fs::write(&target, b"{\"id\":\"br-windows\"}\n").expect("write Windows JSONL source");

        let pinned = pin_jsonl_target(&target).expect("pin Windows JSONL target");
        let opened = pinned
            .open_optional_regular()
            .expect("open Windows JSONL through retained parent")
            .expect("Windows JSONL target should exist");
        assert_eq!(
            opened.identity().volume_serial_number(),
            pinned.parent().identity().volume_serial_number(),
            "file and parent should reside on the same Windows volume"
        );
        assert_eq!(opened.identity().file_index(), opened.identity().inode());

        let snapshot = pinned.capture().expect("capture pinned Windows JSONL");
        assert_eq!(snapshot.identity(), opened.identity());
        let mut contents = String::new();
        snapshot
            .reader()
            .read_to_string(&mut contents)
            .expect("read Windows JSONL snapshot");
        assert_eq!(contents, "{\"id\":\"br-windows\"}\n");
        assert_eq!(
            snapshot.raw_sha256(),
            crate::util::hex_encode(&Sha256::digest(contents.as_bytes()))
        );

        let durability_error = pinned
            .parent()
            .fsync()
            .expect_err("Windows namespace durability must not be falsely certified");
        assert_eq!(durability_error.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            durability_error
                .to_string()
                .contains("cannot certify directory-entry durability")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_jsonl_create_and_link_no_replace_retain_recovery() {
        use std::io::{Read, Write};

        let temp = TempDir::new().expect("create Windows temp directory");
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).expect("create Windows JSONL parent");
        let staged_path = parent.join("issues.jsonl.staged.tmp");
        let output_path = parent.join("issues.jsonl");
        let staged = pin_jsonl_target(&staged_path).expect("pin missing Windows staging leaf");
        let output = staged
            .with_sibling_path(&output_path)
            .expect("derive Windows output sibling");

        let mut staged_file = staged
            .create_new_regular()
            .expect("create Windows staging file without clobber");
        staged_file
            .write_all(b"{\"published\":true}\n")
            .expect("write Windows staging file");
        staged_file.sync_all().expect("sync Windows staging file");
        drop(staged_file);

        let staged_identity = staged
            .capture()
            .expect("capture staged Windows generation")
            .identity();
        let linked_identity = staged
            .link_regular_no_replace_to(&output)
            .expect("atomically link staged generation at missing output");
        assert_eq!(linked_identity, staged_identity);
        assert!(
            staged_path.exists(),
            "staged recovery name must remain until exact by-handle cleanup exists"
        );

        let published = output
            .capture()
            .expect("capture published Windows generation");
        assert_eq!(published.identity(), staged_identity);
        let mut contents = String::new();
        published
            .reader()
            .read_to_string(&mut contents)
            .expect("read published Windows generation");
        assert_eq!(contents, "{\"published\":true}\n");

        let error = staged
            .link_regular_no_replace_to(&output)
            .expect_err("no-replace publication must reject an existing output");
        assert!(
            matches!(error, BeadsError::SyncConflict { .. }),
            "existing Windows output should be a synchronization conflict: {error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_jsonl_leaf_rejects_alternate_stream_and_hashes_raw_utf16() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let temp = TempDir::new().expect("create Windows temp directory");
        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).expect("create Windows JSONL parent");
        let pinned =
            pin_jsonl_target(&parent.join("issues.jsonl")).expect("pin Windows JSONL target");

        let alternate_stream_error = pinned
            .with_leaf(OsStr::new("issues.jsonl:stream"))
            .expect_err("Windows alternate data streams must not be accepted as leaves");
        assert!(
            alternate_stream_error
                .to_string()
                .contains("alternate-data-stream separator")
        );

        let first_leaf = OsString::from_wide(&[
            u16::from(b's'),
            u16::from(b'i'),
            u16::from(b'b'),
            u16::from(b'l'),
            u16::from(b'i'),
            u16::from(b'n'),
            u16::from(b'g'),
            u16::from(b'-'),
            0xd800,
        ]);
        let second_leaf = OsString::from_wide(&[
            u16::from(b's'),
            u16::from(b'i'),
            u16::from(b'b'),
            u16::from(b'l'),
            u16::from(b'i'),
            u16::from(b'n'),
            u16::from(b'g'),
            u16::from(b'-'),
            0xd801,
        ]);
        let first = pinned
            .with_leaf(&first_leaf)
            .expect("derive first raw UTF-16 Windows sibling");
        let second = pinned
            .with_leaf(&second_leaf)
            .expect("derive second raw UTF-16 Windows sibling");
        assert_ne!(first.leaf(), second.leaf());
        assert_ne!(first.leaf_sha256(), second.leaf_sha256());
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_jsonl_rejects_reparse_routes_and_pins_open_leaf() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let temp = TempDir::new().expect("create Windows temp directory");
        let outside = temp.path().join("outside");
        let parent = temp.path().join("parent");
        std::fs::create_dir(&outside).expect("create outside directory");
        std::fs::create_dir(&parent).expect("create Windows JSONL parent");
        let outside_file = outside.join("outside.jsonl");
        std::fs::write(&outside_file, b"{\"outside\":true}\n")
            .expect("write outside Windows JSONL");

        let linked_parent = temp.path().join("linked-parent");
        match symlink_dir(&outside, &linked_parent) {
            Ok(()) => {
                let error = pin_jsonl_target(&linked_parent.join("issues.jsonl"))
                    .expect_err("Windows parent reparse point must be rejected");
                assert!(
                    error.to_string().contains("non-reparse directory"),
                    "unexpected Windows parent-reparse error: {error}"
                );
            }
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!(
                    "skipping Windows directory-symlink assertion: symbolic-link privilege unavailable"
                );
            }
            Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(1314),
                "create Windows directory symlink: {error}"
            ),
        }

        let linked_leaf = parent.join("linked.jsonl");
        match symlink_file(&outside_file, &linked_leaf) {
            Ok(()) => {
                let error = pin_jsonl_target(&linked_leaf)
                    .expect_err("Windows leaf reparse point must be rejected");
                assert!(
                    error
                        .to_string()
                        .contains("without following reparse points"),
                    "unexpected Windows leaf-reparse error: {error}"
                );
            }
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!(
                    "skipping Windows file-symlink assertion: symbolic-link privilege unavailable"
                );
            }
            Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(1314),
                "create Windows file symlink: {error}"
            ),
        }

        let target = parent.join("issues.jsonl");
        let displaced = parent.join("displaced.jsonl");
        std::fs::write(&target, b"{\"pinned\":true}\n").expect("write pinned Windows JSONL");
        let opened =
            open_jsonl_source_nofollow(&target).expect("open pinned Windows JSONL generation");
        let rename_error = std::fs::rename(&target, &displaced)
            .expect_err("read-only pinned handle must deny Windows rename/delete sharing");
        assert!(
            matches!(
                rename_error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
            ),
            "unexpected Windows sharing-violation error: {rename_error}"
        );
        drop(opened);
        std::fs::rename(&target, &displaced)
            .expect("rename should succeed after pinned Windows handle closes");
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
