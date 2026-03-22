//! Configuration management for `beads_rust`.
//!
//! Configuration sources and precedence (highest wins):
//! 1. CLI overrides
//! 2. Environment variables
//! 3. Project config (.beads/config.yaml)
//! 4. User config (~/.config/beads/config.yaml; falls back to ~/.config/bd/config.yaml)
//! 5. Legacy user config (~/.beads/config.yaml)
//! 6. DB config table
//! 7. Defaults

pub mod routing;

use crate::error::{BeadsError, Result, ResultExt};
use crate::model::{IssueType, Priority};
use crate::storage::SqliteStorage;
use crate::sync::{
    ExportConfig, ImportConfig, ImportResult, auto_flush, compute_jsonl_hash,
    export_to_jsonl_with_policy, finalize_export, import_from_jsonl, preflight_import,
};
use crate::util::id::{IdConfig, abbreviate_prefix, split_prefix_remainder};
use chrono::Utc;
use fsqlite_error::FrankenError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tempfile::tempdir;
use tracing::warn;

/// Check whether a directory name is a valid beads directory name.
///
/// Accepts `.beads` (default) and `_beads` (for monorepos that
/// disallow dot-directories).
#[must_use]
pub fn is_beads_dir_name(name: &std::ffi::OsStr) -> bool {
    name == ".beads" || name == "_beads"
}

/// Default database filename used when metadata is missing.
const DEFAULT_DB_FILENAME: &str = "beads.db";
/// Default JSONL filename used when metadata is missing.
const DEFAULT_JSONL_FILENAME: &str = "issues.jsonl";
/// Legacy JSONL filename to fall back to.
const LEGACY_JSONL_FILENAME: &str = "beads.jsonl";
/// Directory used for automatic database recovery backups.
const RECOVERY_DIR_NAME: &str = ".br_recovery";

/// JSONL files that should never be treated as the main export file.
/// Includes merge artifacts, deletion logs, and interaction logs.
const EXCLUDED_JSONL_FILES: &[&str] = &[
    "deletions.jsonl",
    "interactions.jsonl",
    "beads.base.jsonl",
    "beads.left.jsonl",
    "beads.right.jsonl",
    "sync_base.jsonl",
];

/// Startup metadata describing DB + JSONL paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub database: String,
    pub jsonl_export: String,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub deletions_retention_days: Option<u64>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        }
    }
}

impl Metadata {
    /// Load metadata.json from the beads directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(beads_dir: &Path) -> Result<Self> {
        let path = beads_dir.join("metadata.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)?;
        let mut metadata: Self = serde_json::from_str(&contents)?;

        if metadata.database.trim().is_empty() {
            metadata.database = DEFAULT_DB_FILENAME.to_string();
        }
        if metadata.jsonl_export.trim().is_empty() {
            metadata.jsonl_export = DEFAULT_JSONL_FILENAME.to_string();
        }

        Ok(metadata)
    }
}

/// Discover the best JSONL file in the beads directory.
///
/// Selection rules:
/// 1. Prefer `issues.jsonl` if present.
/// 2. Fall back to `beads.jsonl` (legacy) if present.
/// 3. Never use merge artifacts (`beads.base.jsonl`, `beads.left.jsonl`, `beads.right.jsonl`).
/// 4. Never use deletion logs (`deletions.jsonl`) or interaction logs (`interactions.jsonl`).
/// 5. If no valid JSONL exists, return `None` (caller should use default for writing).
#[must_use]
pub fn discover_jsonl(beads_dir: &Path) -> Option<PathBuf> {
    // Check preferred file first
    let issues_path = beads_dir.join(DEFAULT_JSONL_FILENAME);
    if issues_path.is_file() {
        return Some(issues_path);
    }

    // Check legacy file
    let legacy_path = beads_dir.join(LEGACY_JSONL_FILENAME);
    if legacy_path.is_file() {
        return Some(legacy_path);
    }

    // No valid JSONL found
    None
}

/// Check if a JSONL filename should be excluded from discovery.
///
/// Returns `true` for merge artifacts, deletion logs, and interaction logs.
#[must_use]
pub fn is_excluded_jsonl(filename: &str) -> bool {
    Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|basename| EXCLUDED_JSONL_FILES.contains(&basename))
}

/// Resolved paths for this workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub beads_dir: PathBuf,
    pub db_path: PathBuf,
    pub jsonl_path: PathBuf,
    pub metadata: Metadata,
}

impl ConfigPaths {
    /// Resolve database + JSONL paths using metadata and environment overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read.
    pub fn resolve(beads_dir: &Path, db_override: Option<&PathBuf>) -> Result<Self> {
        let metadata = Metadata::load(beads_dir)?;
        let db_path = resolve_db_path(beads_dir, &metadata, db_override);
        let jsonl_path = resolve_jsonl_path(beads_dir, &metadata, db_override);

        Ok(Self {
            beads_dir: beads_dir.to_path_buf(),
            db_path,
            jsonl_path,
            metadata,
        })
    }

    /// Get the user config path (~/.config/beads/config.yaml).
    /// Returns None if HOME is not set.
    #[must_use]
    pub fn user_config_path(&self) -> Option<PathBuf> {
        env::var("HOME").ok().map(|home| {
            let config_root = Path::new(&home).join(".config");
            let beads_path = config_root.join("beads").join("config.yaml");
            if beads_path.exists() {
                beads_path
            } else {
                config_root.join("bd").join("config.yaml")
            }
        })
    }

    /// Get the legacy user config path (~/.beads/config.yaml).
    /// Returns None if HOME is not set.
    #[must_use]
    pub fn legacy_user_config_path(&self) -> Option<PathBuf> {
        env::var("HOME")
            .ok()
            .map(|home| Path::new(&home).join(".beads").join("config.yaml"))
    }

    /// Get the project config path (.beads/config.yaml).
    #[must_use]
    pub fn project_config_path(&self) -> Option<PathBuf> {
        Some(self.beads_dir.join("config.yaml"))
    }
}

/// Discover the active `.beads` directory.
///
/// Honors `BEADS_DIR` when set, otherwise walks up from `start` (or CWD).
///
/// # Errors
///
/// Returns an error if no beads directory is found or the CWD cannot be read.
pub fn discover_beads_dir(start: Option<&Path>) -> Result<PathBuf> {
    discover_beads_dir_with_env(start, None)
}

fn discover_beads_dir_with_env(
    start: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_override {
        return resolve_explicit_beads_dir(path, "beads directory override");
    } else if let Ok(value) = env::var("BEADS_DIR")
        && !value.trim().is_empty()
    {
        let path = PathBuf::from(value);
        return resolve_explicit_beads_dir(&path, "BEADS_DIR");
    }

    let candidate = discover_beads_dir_candidate_with_env(start, None)?;
    routing::follow_redirects(&candidate, 10)
}

fn discover_beads_dir_candidate_with_env(
    start: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_override {
        return validate_explicit_beads_dir(path, "beads directory override");
    } else if let Ok(value) = env::var("BEADS_DIR")
        && !value.trim().is_empty()
    {
        let path = PathBuf::from(value);
        return validate_explicit_beads_dir(&path, "BEADS_DIR");
    }

    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => env::current_dir()?,
    };

    loop {
        let candidate = current.join(".beads");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        let candidate_underscore = current.join("_beads");
        if candidate_underscore.is_dir() {
            return Ok(candidate_underscore);
        }

        if !current.pop() {
            break;
        }
    }

    Err(BeadsError::NotInitialized)
}

/// Discover beads directory, using `--db` path if provided.
///
/// When `--db` is explicitly provided and the path itself lives under `.beads/`,
/// derives the beads_dir from that path (e.g., `/path/to/.beads/beads.db` →
/// `/path/to/.beads/`), allowing br to work from any directory.
///
/// For external database overrides that live outside `.beads/`, falls back to
/// normal workspace discovery so commands can still use the current project's
/// metadata/config while targeting the explicit database file.
///
/// # Errors
///
/// Returns an error if:
/// - `--db` path is external and no workspace can be discovered from CWD/BEADS_DIR
/// - No beads directory found (when `--db` not provided)
pub fn discover_beads_dir_with_cli(cli: &CliOverrides) -> Result<PathBuf> {
    discover_beads_dir_with_cli_from(None, cli, None, None)
}

/// Discover the active `.beads` directory, but allow "no workspace" when no
/// explicit `--db` target was provided.
///
/// This is intended for commands that can operate outside a project and should
/// only suppress `NotInitialized` when the user did not explicitly point to a
/// database.
///
/// # Errors
///
/// Returns an error when:
/// - An explicit `--db` path is invalid
/// - Discovery fails for reasons other than `NotInitialized`
pub fn discover_optional_beads_dir_with_cli(cli: &CliOverrides) -> Result<Option<PathBuf>> {
    match discover_beads_dir_with_cli_from(None, cli, None, None) {
        Ok(path) => Ok(Some(path)),
        Err(BeadsError::NotInitialized) if cli.db.is_none() => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn discover_optional_beads_dir_candidate_with_cli(
    cli: &CliOverrides,
) -> Result<Option<PathBuf>> {
    match discover_beads_dir_candidate_with_cli_from(None, cli, None, None) {
        Ok(path) => Ok(Some(path)),
        Err(BeadsError::NotInitialized) if cli.db.is_none() => Ok(None),
        Err(err) => Err(err),
    }
}

fn discover_beads_dir_with_cli_from(
    start: Option<&Path>,
    cli: &CliOverrides,
    beads_dir_env_override: Option<&Path>,
    db_env_override: Option<&Path>,
) -> Result<PathBuf> {
    let explicit_external_cli_db = cli
        .db
        .as_deref()
        .filter(|db_path| beads_dir_from_db_path(db_path).is_none());

    if let Some(db_path) = cli.db.as_deref()
        && let Some(beads_dir) = beads_dir_from_db_path(db_path)
    {
        return resolve_explicit_beads_dir(
            &beads_dir,
            &format!("database override '{}'", db_path.display()),
        );
    }

    let startup_db_override = db_env_override
        .map(Path::to_path_buf)
        .or_else(startup_db_override_from_env);

    if let Some(db_path) = startup_db_override.as_deref()
        && let Ok(beads_dir) = derive_beads_dir_from_db_path(db_path)
    {
        return resolve_explicit_beads_dir(
            &beads_dir,
            &format!("database override '{}'", db_path.display()),
        );
    }

    discover_beads_dir_with_env(start, beads_dir_env_override).map_err(
        |err| match (
            err,
            explicit_external_cli_db.or(startup_db_override.as_deref()),
        ) {
            (BeadsError::NotInitialized, Some(db_path)) => BeadsError::WithContext {
                context: format!(
                    "Cannot resolve the project .beads directory for database override '{}'; run from the target workspace or set BEADS_DIR",
                    db_path.display()
                ),
                source: Box::new(BeadsError::NotInitialized),
            },
            (err, _) => err,
        },
    )
}

fn discover_beads_dir_candidate_with_cli_from(
    start: Option<&Path>,
    cli: &CliOverrides,
    beads_dir_env_override: Option<&Path>,
    db_env_override: Option<&Path>,
) -> Result<PathBuf> {
    let explicit_external_cli_db = cli
        .db
        .as_deref()
        .filter(|db_path| beads_dir_from_db_path(db_path).is_none());

    if let Some(db_path) = cli.db.as_deref()
        && let Some(beads_dir) = beads_dir_from_db_path(db_path)
    {
        return validate_explicit_beads_dir(
            &beads_dir,
            &format!("database override '{}'", db_path.display()),
        );
    }

    let startup_db_override = db_env_override
        .map(Path::to_path_buf)
        .or_else(startup_db_override_from_env);

    if let Some(db_path) = startup_db_override.as_deref()
        && let Ok(beads_dir) = derive_beads_dir_from_db_path(db_path)
    {
        return validate_explicit_beads_dir(
            &beads_dir,
            &format!("database override '{}'", db_path.display()),
        );
    }

    discover_beads_dir_candidate_with_env(start, beads_dir_env_override).map_err(
        |err| match (
            err,
            explicit_external_cli_db.or(startup_db_override.as_deref()),
        ) {
            (BeadsError::NotInitialized, Some(db_path)) => BeadsError::WithContext {
                context: format!(
                    "Cannot resolve the project .beads directory for database override '{}'; run from the target workspace or set BEADS_DIR",
                    db_path.display()
                ),
                source: Box::new(BeadsError::NotInitialized),
            },
            (err, _) => err,
        },
    )
}

fn startup_db_override_from_env() -> Option<PathBuf> {
    for key in ["BD_DB", "BD_DATABASE"] {
        if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// Extract the `.beads/` directory from a database path.
///
/// E.g., `/path/to/.beads/beads.db` → `/path/to/.beads/`
fn derive_beads_dir_from_db_path(db_path: &Path) -> Result<PathBuf> {
    beads_dir_from_db_path(db_path).ok_or_else(|| {
        BeadsError::validation(
            "db",
            format!(
                "Cannot derive beads directory from path '{}': expected path to contain '.beads/' component",
                db_path.display()
            ),
        )
    })
}

fn validate_explicit_beads_dir(path: &Path, source: &str) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(BeadsError::Config(format!(
            "{source} not found or not a .beads directory: {}",
            path.display()
        )));
    }

    Ok(path.to_path_buf())
}

fn resolve_explicit_beads_dir(path: &Path, source: &str) -> Result<PathBuf> {
    let candidate = validate_explicit_beads_dir(path, source)?;
    routing::follow_redirects(&candidate, 10).map_err(|err| BeadsError::WithContext {
        context: format!("{source} is invalid"),
        source: Box::new(err),
    })
}

fn beads_dir_from_db_path(db_path: &Path) -> Option<PathBuf> {
    let mut current = db_path.to_path_buf();

    if current.file_name().is_some_and(is_beads_dir_name) {
        return Some(current);
    }

    if current.is_file() {
        current.pop();
        if current.file_name().is_some_and(is_beads_dir_name) {
            return Some(current);
        }
    }

    db_path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(is_beads_dir_name))
        .map(Path::to_path_buf)
}

#[derive(Debug)]
struct RecoveryBackupSet {
    db_path: PathBuf,
    recovery_dir: PathBuf,
    stamp: String,
    files: Vec<(PathBuf, PathBuf)>,
}

fn open_sqlite_storage_with_recovery(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
) -> Result<SqliteStorage> {
    if !paths.db_path.is_file() && paths.jsonl_path.is_file() {
        return rebuild_database_from_jsonl(beads_dir, paths, lock_timeout, bootstrap_layer);
    }

    match SqliteStorage::open_with_timeout(&paths.db_path, lock_timeout) {
        Ok(storage) => match storage.detect_recoverable_open_anomaly() {
            Ok(None) => Ok(storage),
            Ok(Some(anomaly)) => {
                drop(storage);
                warn!(
                    db_path = %paths.db_path.display(),
                    jsonl_path = %paths.jsonl_path.display(),
                    anomaly = %anomaly,
                    "Detected recoverable database anomaly after open; rebuilding from JSONL"
                );
                rebuild_database_from_jsonl(beads_dir, paths, lock_timeout, bootstrap_layer)
            }
            Err(probe_err) => {
                drop(storage);
                if !should_attempt_jsonl_recovery_after_open(
                    &probe_err,
                    &paths.db_path,
                    &paths.jsonl_path,
                ) {
                    return Err(probe_err);
                }

                warn!(
                    db_path = %paths.db_path.display(),
                    jsonl_path = %paths.jsonl_path.display(),
                    probe_error = %probe_err,
                    "Post-open database probe failed; rebuilding from JSONL"
                );
                rebuild_database_from_jsonl(beads_dir, paths, lock_timeout, bootstrap_layer)
            }
        },
        Err(open_err) => {
            if !should_attempt_jsonl_recovery(&open_err, &paths.db_path, &paths.jsonl_path) {
                return Err(open_err);
            }

            match rebuild_database_from_jsonl(beads_dir, paths, lock_timeout, bootstrap_layer) {
                Ok(storage) => Ok(storage),
                Err(recovery_err) => {
                    warn!(
                        db_path = %paths.db_path.display(),
                        jsonl_path = %paths.jsonl_path.display(),
                        open_error = %open_err,
                        recovery_error = %recovery_err,
                        "Automatic database recovery from JSONL failed"
                    );
                    if should_surface_recovery_error(&recovery_err) {
                        Err(recovery_err)
                    } else {
                        Err(open_err)
                    }
                }
            }
        }
    }
}

fn should_attempt_jsonl_recovery(open_err: &BeadsError, db_path: &Path, jsonl_path: &Path) -> bool {
    if !db_path.is_file() || !jsonl_path.is_file() {
        return false;
    }

    matches!(
        open_err,
        BeadsError::Database(
            FrankenError::DatabaseCorrupt { .. }
                | FrankenError::NotADatabase { .. }
                | FrankenError::WalCorrupt { .. }
                | FrankenError::ShortRead { .. }
                | FrankenError::TableExists { .. }
                | FrankenError::IndexExists { .. }
        )
    ) || matches!(
        open_err,
        BeadsError::Database(FrankenError::Internal(detail))
            if is_recoverable_database_internal_error(detail)
    )
}

fn should_attempt_jsonl_recovery_after_open(
    probe_err: &BeadsError,
    db_path: &Path,
    jsonl_path: &Path,
) -> bool {
    should_attempt_jsonl_recovery(probe_err, db_path, jsonl_path)
        || matches!(
            probe_err,
            BeadsError::Database(FrankenError::QueryReturnedMultipleRows)
        )
}

fn is_duplicate_schema_entry_open_error(detail: &str) -> bool {
    let detail = detail.trim();
    let detail_lower = detail.to_ascii_lowercase();

    detail_lower.contains("malformed database schema")
        || detail_lower
            .strip_prefix("table ")
            .is_some_and(|rest| rest.ends_with(" already exists"))
        || detail_lower
            .strip_prefix("index ")
            .is_some_and(|rest| rest.ends_with(" already exists"))
}

fn is_recoverable_database_internal_error(detail: &str) -> bool {
    let detail_lower = detail.trim().to_ascii_lowercase();

    is_duplicate_schema_entry_open_error(detail)
        || detail_lower.contains("database disk image is malformed")
        || detail_lower.contains("malformed database disk image")
        || detail_lower.contains("missing from index")
}

fn rebuild_database_from_jsonl(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
) -> Result<SqliteStorage> {
    repair_database_from_jsonl(
        beads_dir,
        &paths.db_path,
        &paths.jsonl_path,
        lock_timeout,
        bootstrap_layer,
        false,
    )
    .map(|(storage, _)| storage)
}

pub(crate) fn repair_database_from_jsonl(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    show_progress: bool,
) -> Result<(SqliteStorage, ImportResult)> {
    let prefix = resolve_bootstrap_issue_prefix(bootstrap_layer, beads_dir, jsonl_path)?;
    let mut import_config = import_config_for_resolved_jsonl(beads_dir, db_path, jsonl_path);
    import_config.show_progress = show_progress;
    import_config.skip_prefix_validation = true;

    preflight_import(jsonl_path, &import_config, Some(&prefix))?.into_result()?;

    warn!(
        db_path = %db_path.display(),
        jsonl_path = %jsonl_path.display(),
        "Rebuilding SQLite database from JSONL"
    );

    let ((storage, import_result), recovery_dir) =
        rebuild_database_family_with_backup(db_path, beads_dir, || {
            rebuild_database_family(db_path, lock_timeout, jsonl_path, &import_config, &prefix)
        })?;

    warn!(
        db_path = %db_path.display(),
        recovery_dir = %recovery_dir.display(),
        "SQLite rebuild from JSONL succeeded"
    );
    Ok((storage, import_result))
}

fn should_surface_recovery_error(recovery_err: &BeadsError) -> bool {
    matches!(recovery_err, BeadsError::WithContext { .. })
}

fn recovery_restore_failure(
    backup_set: &RecoveryBackupSet,
    recovery_err: &BeadsError,
    restore_err: BeadsError,
) -> BeadsError {
    BeadsError::WithContext {
        context: format!(
            "Automatic database recovery failed ({recovery_err}); original database restore from '{}' also failed",
            backup_set.recovery_dir.display()
        ),
        source: Box::new(restore_err),
    }
}

fn rollback_renamed_paths(renamed_paths: &[(PathBuf, PathBuf)], operation: &str) -> Result<()> {
    for (original, renamed) in renamed_paths.iter().rev() {
        fs::rename(renamed, original).with_context(|| {
            format!(
                "Failed to roll back {operation}: restore '{}' from '{}'",
                original.display(),
                renamed.display()
            )
        })?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingRenameSourcePolicy {
    Skip,
    Error,
}

fn rename_existing_paths<I>(
    paths: I,
    operation: &str,
    missing_source_policy: MissingRenameSourcePolicy,
) -> Result<Vec<(PathBuf, PathBuf)>>
where
    I: IntoIterator<Item = (PathBuf, PathBuf)>,
{
    let mut renamed_paths = Vec::new();

    for (original, renamed) in paths {
        match fs::symlink_metadata(&original) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if matches!(missing_source_policy, MissingRenameSourcePolicy::Skip) {
                    continue;
                }

                let rename_err = BeadsError::WithContext {
                    context: format!("Failed to {operation}"),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("expected '{}' to exist", original.display()),
                    )),
                };
                if let Err(rollback_err) = rollback_renamed_paths(&renamed_paths, operation) {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Failed to {operation} ({rename_err}); rollback also failed"
                        ),
                        source: Box::new(rollback_err),
                    });
                }

                return Err(rename_err);
            }
            Err(err) => {
                let rename_err = BeadsError::WithContext {
                    context: format!(
                        "Failed to inspect '{}' before attempting to {operation}",
                        original.display()
                    ),
                    source: Box::new(err),
                };
                if let Err(rollback_err) = rollback_renamed_paths(&renamed_paths, operation) {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Failed to {operation} ({rename_err}); rollback also failed"
                        ),
                        source: Box::new(rollback_err),
                    });
                }

                return Err(rename_err);
            }
        }

        if let Err(rename_err) = fs::rename(&original, &renamed) {
            if let Err(rollback_err) = rollback_renamed_paths(&renamed_paths, operation) {
                warn!(
                    operation,
                    rollback_error = %rollback_err,
                    "Failed to roll back partially completed file rename batch"
                );
                return Err(BeadsError::WithContext {
                    context: format!("Failed to {operation} ({rename_err}); rollback also failed"),
                    source: Box::new(rollback_err),
                });
            }

            return Err(rename_err.into());
        }

        renamed_paths.push((original, renamed));
    }

    Ok(renamed_paths)
}

fn rebuild_database_family(
    db_path: &Path,
    lock_timeout: Option<u64>,
    jsonl_path: &Path,
    import_config: &ImportConfig,
    prefix: &str,
) -> Result<(SqliteStorage, ImportResult)> {
    let mut storage = SqliteStorage::open_with_timeout(db_path, lock_timeout)?;
    storage.set_config("issue_prefix", prefix)?;
    let import_result = import_from_jsonl(&mut storage, jsonl_path, import_config, Some(prefix))?;
    Ok((storage, import_result))
}

pub(crate) fn rebuild_database_family_with_backup<T, F>(
    db_path: &Path,
    beads_dir: &Path,
    rebuild: F,
) -> Result<(T, PathBuf)>
where
    F: FnOnce() -> Result<T>,
{
    let backup_set = backup_database_family_for_recovery(db_path, beads_dir)?;
    let recovery_dir = backup_set.recovery_dir.clone();

    match rebuild() {
        Ok(value) => Ok((value, recovery_dir)),
        Err(rebuild_err) => {
            if let Err(restore_err) = restore_database_family_after_failed_rebuild(&backup_set) {
                warn!(
                    db_path = %db_path.display(),
                    recovery_dir = %backup_set.recovery_dir.display(),
                    restore_error = %restore_err,
                    "Failed to restore original database after unsuccessful rebuild"
                );
                return Err(recovery_restore_failure(
                    &backup_set,
                    &rebuild_err,
                    restore_err,
                ));
            }
            Err(rebuild_err)
        }
    }
}

fn backup_database_family_for_recovery(
    db_path: &Path,
    beads_dir: &Path,
) -> Result<RecoveryBackupSet> {
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
    move_database_family_to_recovery(db_path, beads_dir, &stamp)
}

fn move_database_family_to_recovery(
    db_path: &Path,
    beads_dir: &Path,
    stamp: &str,
) -> Result<RecoveryBackupSet> {
    let recovery_dir = recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;
    let files = rename_existing_paths(
        database_family_paths(db_path).into_iter().map(|original| {
            let backup = recovery_dir.join(recovery_backup_filename(&original, stamp, "bak"));
            (original, backup)
        }),
        "move the database family into recovery",
        MissingRenameSourcePolicy::Skip,
    )?;

    Ok(RecoveryBackupSet {
        db_path: db_path.to_path_buf(),
        recovery_dir,
        stamp: stamp.to_string(),
        files,
    })
}

fn restore_database_family_after_failed_rebuild(backup_set: &RecoveryBackupSet) -> Result<()> {
    let rebuilt_backups = rename_existing_paths(
        database_family_paths(&backup_set.db_path)
            .into_iter()
            .map(|rebuilt| {
                let failed_backup = backup_set.recovery_dir.join(recovery_backup_filename(
                    &rebuilt,
                    &backup_set.stamp,
                    "rebuild-failed",
                ));
                (rebuilt, failed_backup)
            }),
        "stage rebuilt database files after failed recovery",
        MissingRenameSourcePolicy::Skip,
    )?;

    if let Err(restore_err) = rename_existing_paths(
        backup_set
            .files
            .iter()
            .map(|(original, backup)| (backup.clone(), original.clone())),
        "restore the original database family after failed recovery",
        MissingRenameSourcePolicy::Error,
    ) {
        if let Err(rollback_err) = rollback_renamed_paths(
            &rebuilt_backups,
            "restore the original database family after failed recovery",
        ) {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to restore the original database family ({restore_err}); \
                     rolling staged rebuilt files back into place also failed"
                ),
                source: Box::new(rollback_err),
            });
        }

        return Err(restore_err);
    }

    Ok(())
}

pub(crate) fn recovery_dir_for_db_path(db_path: &Path, beads_dir: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(beads_dir)
        .join(RECOVERY_DIR_NAME)
}

fn database_family_paths(db_path: &Path) -> Vec<PathBuf> {
    let db_string = db_path.to_string_lossy();
    vec![
        db_path.to_path_buf(),
        PathBuf::from(format!("{db_string}-wal")),
        PathBuf::from(format!("{db_string}-shm")),
        PathBuf::from(format!("{db_string}-journal")),
    ]
}

fn copy_database_family_to_directory(db_path: &Path, destination_dir: &Path) -> Result<PathBuf> {
    let snapshot_db_name = db_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(DEFAULT_DB_FILENAME));
    let snapshot_db_path = destination_dir.join(snapshot_db_name);

    for original in database_family_paths(db_path) {
        match fs::symlink_metadata(&original) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BeadsError::Config(format!(
                    "Database snapshot source '{}' must not be a symlink",
                    original.display()
                )));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(BeadsError::Config(format!(
                    "Database snapshot source '{}' must be a regular file",
                    original.display()
                )));
            }
            Ok(_) => {
                let snapshot_path = destination_dir.join(
                    original
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(DEFAULT_DB_FILENAME)),
                );
                fs::copy(&original, &snapshot_path).with_context(|| {
                    format!(
                        "Failed to copy database snapshot artifact '{}' to '{}'",
                        original.display(),
                        snapshot_path.display()
                    )
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if original == db_path {
                    return Err(err.into());
                }
            }
            Err(err) => {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Failed to inspect database snapshot source '{}'",
                        original.display()
                    ),
                    source: Box::new(err),
                });
            }
        }
    }

    Ok(snapshot_db_path)
}

pub(crate) fn with_database_family_snapshot<T, F>(db_path: &Path, read: F) -> Result<T>
where
    F: FnOnce(&Path) -> Result<T>,
{
    let snapshot_dir = tempdir().with_context(|| {
        format!(
            "Failed to create a temporary directory for the database snapshot '{}'",
            db_path.display()
        )
    })?;
    let snapshot_db_path = copy_database_family_to_directory(db_path, snapshot_dir.path())?;
    read(&snapshot_db_path)
}

fn recovery_backup_filename(path: &Path, stamp: &str, suffix: &str) -> String {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("beads.db");
    format!("{filename}.{stamp}.{suffix}")
}

pub(crate) fn quarantine_database_artifacts<I>(
    db_path: &Path,
    beads_dir: &Path,
    artifact_paths: I,
    suffix: &str,
) -> Result<Vec<PathBuf>>
where
    I: IntoIterator<Item = PathBuf>,
{
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
    let recovery_dir = recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;

    let renamed_paths = rename_existing_paths(
        artifact_paths.into_iter().map(|original| {
            let backup = recovery_dir.join(recovery_backup_filename(&original, &stamp, suffix));
            (original, backup)
        }),
        "quarantine database artifacts",
        MissingRenameSourcePolicy::Skip,
    )?;

    Ok(renamed_paths
        .into_iter()
        .map(|(_, backup)| backup)
        .collect())
}

/// Open storage using resolved config paths, returning the storage and paths used.
///
/// # Errors
///
/// Returns an error if metadata cannot be read or the database cannot be opened.
pub fn open_storage(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
    lock_timeout: Option<u64>,
) -> Result<(SqliteStorage, ConfigPaths)> {
    let startup = load_startup_config_with_paths(beads_dir, db_override)?;
    let merged_layer = ConfigLayer::merge_layers(&startup.layers);

    let resolved_lock_timeout = lock_timeout
        .or_else(|| lock_timeout_from_layer(&merged_layer))
        .or(Some(30000));

    let storage = open_sqlite_storage_with_recovery(
        beads_dir,
        &startup.paths,
        resolved_lock_timeout,
        &merged_layer,
    )?;
    Ok((storage, startup.paths))
}

/// Storage handle with no-db awareness.
#[derive(Debug)]
pub struct OpenStorageResult {
    pub storage: SqliteStorage,
    pub paths: ConfigPaths,
    pub no_db: bool,
    allow_external_jsonl: bool,
    startup_layers: Vec<ConfigLayer>,
    bootstrap_layer: ConfigLayer,
    resolved_lock_timeout: Option<u64>,
    loaded_jsonl_hash: Option<String>,
}

impl OpenStorageResult {
    /// Load the full merged config while reusing startup layers already read
    /// during storage resolution.
    ///
    /// # Errors
    ///
    /// Returns an error if JSONL prefix inference or DB-backed config loading fails.
    pub fn load_config(&self, cli: &CliOverrides) -> Result<ConfigLayer> {
        load_config_from_startup_layers(
            &self.startup_layers,
            &self.paths.jsonl_path,
            Some(&self.storage),
            cli,
        )
    }

    #[must_use]
    pub(crate) fn should_attempt_jsonl_recovery(&self, err: &BeadsError) -> bool {
        !self.no_db
            && should_attempt_jsonl_recovery(err, &self.paths.db_path, &self.paths.jsonl_path)
    }

    /// Rebuild the current SQLite database from the resolved JSONL export.
    ///
    /// # Errors
    ///
    /// Returns an error if recovery fails or if this context is in `--no-db`
    /// mode.
    pub(crate) fn recover_database_from_jsonl(&mut self) -> Result<()> {
        if self.no_db {
            return Err(BeadsError::Config(
                "cannot rebuild SQLite database from JSONL while --no-db mode is active"
                    .to_string(),
            ));
        }

        // Close the old connection before rebuilding at the same path.
        // fsqlite tracks pages by file path, so keeping the old connection
        // open while creating a new database at the same path causes
        // BusySnapshot conflicts.
        self.storage = SqliteStorage::open_memory()?;

        let (storage, _) = repair_database_from_jsonl(
            &self.paths.beads_dir,
            &self.paths.db_path,
            &self.paths.jsonl_path,
            self.resolved_lock_timeout,
            &self.bootstrap_layer,
            false,
        )?;
        self.storage = storage;
        self.loaded_jsonl_hash = None;
        Ok(())
    }

    /// Flush JSONL if no-db mode is enabled and there are pending changes.
    ///
    /// Refuses to export if the on-disk JSONL changed since this no-db session
    /// loaded its snapshot. Re-importing into the same dirty in-memory storage
    /// is not a safe merge and can overwrite local edits.
    ///
    /// # Errors
    ///
    /// Returns an error if concurrent JSONL changes are detected or export fails.
    pub fn flush_no_db_if_dirty(&mut self) -> Result<()> {
        if !self.no_db {
            return Ok(());
        }

        let dirty_issue_count = self.storage.get_dirty_issue_count()?;
        let needs_flush = self.storage.get_metadata("needs_flush")?.as_deref() == Some("true");

        if dirty_issue_count == 0 && !needs_flush {
            return Ok(());
        }

        let current_jsonl_hash = if self.paths.jsonl_path.is_file() {
            Some(compute_jsonl_hash(&self.paths.jsonl_path)?)
        } else {
            None
        };

        if current_jsonl_hash != self.loaded_jsonl_hash {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "JSONL changed on disk since this --no-db session started: {}\n\
                     Refusing to flush a stale in-memory snapshot because it could overwrite \
                     concurrent changes.\n\
                     Hint: rerun the command against the latest JSONL, or use `br sync` to \
                     reconcile competing edits explicitly.",
                    self.paths.jsonl_path.display()
                ),
            });
        }

        let export_config = ExportConfig {
            // When needs_flush is set (e.g. after purge_issue), force must be
            // true even if there are also dirty issues from related mutations
            // (like dependency removal during --hard delete), so the safety
            // guard does not block export of a DB that intentionally has fewer
            // issues than the on-disk JSONL.
            force: needs_flush,
            is_default_path: self.paths.jsonl_path == self.paths.beads_dir.join("issues.jsonl"),
            beads_dir: Some(self.paths.beads_dir.clone()),
            allow_external_jsonl: self.allow_external_jsonl,
            show_progress: false,
            ..Default::default()
        };

        let (export_result, _report) =
            export_to_jsonl_with_policy(&self.storage, &self.paths.jsonl_path, &export_config)?;
        finalize_export(
            &mut self.storage,
            &export_result,
            Some(&export_result.issue_hashes),
            &self.paths.jsonl_path,
        )?;
        self.loaded_jsonl_hash = Some(export_result.content_hash);

        Ok(())
    }

    /// Persist no-db changes before rendering success output.
    ///
    /// This prevents commands from emitting success or machine-readable output
    /// for mutations that later fail during no-db JSONL flush.
    ///
    /// # Errors
    ///
    /// Returns any no-db flush error before invoking `on_success`, or any error
    /// returned by `on_success` after persistence succeeds.
    pub fn flush_no_db_then<T, F>(&mut self, on_success: F) -> Result<T>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        self.flush_no_db_if_dirty()?;
        on_success(self)
    }

    /// Attempt SQLite auto-flush for this storage context when enabled.
    ///
    /// This mirrors the main-process post-command auto-flush policy so routed
    /// external-project mutations can export their own JSONL instead of relying
    /// on the caller's local workspace context.
    ///
    /// # Errors
    ///
    /// Returns any export error encountered while auto-flush is enabled.
    pub fn auto_flush_if_enabled(&mut self) -> Result<()> {
        if self.no_db || no_auto_flush_from_layer(&self.bootstrap_layer).unwrap_or(false) {
            return Ok(());
        }

        auto_flush(
            &mut self.storage,
            &self.paths.beads_dir,
            &self.paths.jsonl_path,
            self.allow_external_jsonl,
        )?;
        Ok(())
    }
}

/// Open storage with CLI overrides and support for `--no-db` mode.
///
/// # Errors
///
/// Returns an error if configuration loading, JSONL import, or storage setup fails.
pub fn open_storage_with_cli(beads_dir: &Path, cli: &CliOverrides) -> Result<OpenStorageResult> {
    let startup = load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let StartupConfig {
        paths,
        layers: startup_layers,
        ..
    } = startup;
    let cli_layer = cli.as_layer();

    let mut all_layers = startup_layers.clone();
    all_layers.push(cli_layer);
    let merged_layer = ConfigLayer::merge_layers(&all_layers);

    let no_db = no_db_from_layer(&merged_layer).unwrap_or(false);
    let allow_external_jsonl =
        implicit_external_jsonl_allowed(beads_dir, &paths.db_path, &paths.jsonl_path);

    let resolved_lock_timeout = cli
        .lock_timeout
        .or_else(|| lock_timeout_from_layer(&merged_layer))
        .or(Some(30000));

    if no_db {
        let mut storage = SqliteStorage::open_memory()?;
        let prefix = resolve_bootstrap_issue_prefix(&merged_layer, beads_dir, &paths.jsonl_path)?;
        storage.set_config("issue_prefix", &prefix)?;

        if paths.jsonl_path.is_file() {
            let mut import_config =
                import_config_for_resolved_jsonl(beads_dir, &paths.db_path, &paths.jsonl_path);
            import_config.skip_prefix_validation = true;
            import_from_jsonl(
                &mut storage,
                &paths.jsonl_path,
                &import_config,
                Some(&prefix),
            )?;
        }
        let loaded_jsonl_hash = if paths.jsonl_path.is_file() {
            Some(compute_jsonl_hash(&paths.jsonl_path)?)
        } else {
            None
        };

        Ok(OpenStorageResult {
            storage,
            paths,
            no_db,
            allow_external_jsonl,
            startup_layers,
            bootstrap_layer: merged_layer,
            resolved_lock_timeout,
            loaded_jsonl_hash,
        })
    } else {
        let storage = open_sqlite_storage_with_recovery(
            beads_dir,
            &paths,
            resolved_lock_timeout,
            &merged_layer,
        )?;
        Ok(OpenStorageResult {
            storage,
            paths,
            no_db,
            allow_external_jsonl,
            startup_layers,
            bootstrap_layer: merged_layer,
            resolved_lock_timeout,
            loaded_jsonl_hash: None,
        })
    }
}

#[must_use]
pub fn no_db_from_layer(layer: &ConfigLayer) -> Option<bool> {
    get_startup_value(layer, &["no-db", "no_db", "no.db"]).and_then(|value| parse_bool(value))
}

/// Check merged config for `sync.auto_flush` (inverted) or legacy `no-auto-flush`.
///
/// Priority order (highest first):
/// 1. `sync.auto_flush` / `sync.auto-flush` / `sync.auto.flush` — canonical positive key
/// 2. `no-auto-flush` / `no_auto_flush` / `no.auto.flush` — legacy inverted key
///
/// The canonical `sync.auto_flush = false` means "disable auto-flush".  The legacy
/// `no-auto-flush = true` means the same thing.  By checking the canonical key first,
/// a project config with `sync.auto_flush: false` wins even when the merged layer
/// happens to contain a stale legacy `no-auto-flush` entry.
#[must_use]
pub fn no_auto_flush_from_layer(layer: &ConfigLayer) -> Option<bool> {
    // Canonical key: sync.auto_flush (false => no_auto_flush = true)
    if let Some(v) = get_startup_value(
        layer,
        &["sync.auto_flush", "sync.auto-flush", "sync.auto.flush"],
    )
    .and_then(|value| parse_bool(value))
    {
        return Some(!v);
    }
    // Legacy key: no-auto-flush / no_auto_flush / no.auto.flush
    get_startup_value(layer, &["no-auto-flush", "no_auto_flush", "no.auto.flush"])
        .and_then(|value| parse_bool(value))
}

/// Check merged config for `sync.auto_import` (inverted) or legacy `no-auto-import`.
///
/// Priority order (highest first):
/// 1. `sync.auto_import` / `sync.auto-import` / `sync.auto.import` — canonical positive key
/// 2. `no-auto-import` / `no_auto_import` / `no.auto.import` — legacy inverted key
#[must_use]
pub fn no_auto_import_from_layer(layer: &ConfigLayer) -> Option<bool> {
    // Canonical key: sync.auto_import (false => no_auto_import = true)
    if let Some(v) = get_startup_value(
        layer,
        &["sync.auto_import", "sync.auto-import", "sync.auto.import"],
    )
    .and_then(|value| parse_bool(value))
    {
        return Some(!v);
    }
    // Legacy key: no-auto-import / no_auto_import / no.auto.import
    get_startup_value(
        layer,
        &["no-auto-import", "no_auto_import", "no.auto.import"],
    )
    .and_then(|value| parse_bool(value))
}

fn resolve_bootstrap_issue_prefix(
    bootstrap_layer: &ConfigLayer,
    beads_dir: &Path,
    jsonl_path: &Path,
) -> Result<String> {
    if let Some(prefix) = get_value(bootstrap_layer, &["issue_prefix", "issue-prefix", "prefix"]) {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    if let Some(prefix) = first_prefix_from_jsonl(jsonl_path)? {
        return Ok(prefix);
    }

    if let Some(name) = beads_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(abbreviate_prefix(name));
    }

    Ok("br".to_string())
}

fn import_config_for_resolved_jsonl(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
) -> ImportConfig {
    ImportConfig {
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl: implicit_external_jsonl_allowed(beads_dir, db_path, jsonl_path),
        show_progress: false,
        ..Default::default()
    }
}

pub(crate) fn resolved_jsonl_path_is_external(beads_dir: &Path, jsonl_path: &Path) -> bool {
    !path_is_within_beads_dir(jsonl_path, beads_dir)
}

/// Return whether an external JSONL path can be trusted implicitly without
/// a command-level `--allow-external-jsonl` opt-in.
///
/// This is only allowed when the database itself also lives outside `.beads/`
/// and the JSONL is its sibling, which covers explicit external DB families
/// without allowing ambient `BEADS_JSONL` or metadata overrides to bypass the
/// external-path safety model.
#[must_use]
pub fn implicit_external_jsonl_allowed(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
) -> bool {
    resolved_jsonl_path_is_external(beads_dir, jsonl_path)
        && !path_is_within_beads_dir(db_path, beads_dir)
        && db_path.parent().is_some()
        && db_path.parent() == jsonl_path.parent()
}

fn path_is_within_beads_dir(path: &Path, beads_dir: &Path) -> bool {
    let canonical_beads =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());

    let effective_path = if path.exists() {
        dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else if let Some(parent) = path.parent().filter(|parent| parent.exists()) {
        let canonical_parent = dunce::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        path.file_name().map_or_else(
            || canonical_parent.clone(),
            |name| canonical_parent.join(name),
        )
    } else {
        path.to_path_buf()
    };

    effective_path.starts_with(beads_dir) || effective_path.starts_with(&canonical_beads)
}

/// Fast prefix inference: reads only the first issue from JSONL.
/// Used by `load_config` on every command — must be O(1) not O(n).
pub(crate) fn first_prefix_from_jsonl(jsonl_path: &Path) -> Result<Option<String>> {
    if !jsonl_path.is_file() {
        return Ok(None);
    }

    let file = std::fs::File::open(jsonl_path)?;
    let reader = std::io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip tombstones — they may retain a foreign prefix from before
        // a prefix migration and should not influence inference.
        if value
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "tombstone")
        {
            continue;
        }

        let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((prefix, _)) = split_prefix_remainder(id) else {
            continue;
        };
        if !prefix.is_empty() {
            return Ok(Some(prefix.to_string()));
        }
    }

    Ok(None)
}

/// Resolve config paths using startup config layers for overrides.
///
/// # Errors
///
/// Returns an error if startup config cannot be read or metadata cannot be loaded.
pub fn resolve_paths(beads_dir: &Path, db_override: Option<&PathBuf>) -> Result<ConfigPaths> {
    let startup = load_startup_config_with_paths(beads_dir, db_override)?;
    Ok(startup.paths)
}

fn resolve_db_path(
    beads_dir: &Path,
    metadata: &Metadata,
    db_override: Option<&PathBuf>,
) -> PathBuf {
    if let Some(override_path) = db_override {
        return override_path.clone();
    }

    let candidate = PathBuf::from(&metadata.database);
    if candidate.is_absolute() {
        candidate
    } else {
        // Use BEADS_CACHE_DIR if set, otherwise beads_dir
        // This allows storing the database on a fast local filesystem
        // when .beads is on a slow network mount
        crate::util::resolve_cache_dir(beads_dir).join(candidate)
    }
}

fn resolve_jsonl_path(
    beads_dir: &Path,
    metadata: &Metadata,
    db_override: Option<&PathBuf>,
) -> PathBuf {
    // Priority 1: BEADS_JSONL environment variable (highest priority)
    if let Ok(env_path) = env::var("BEADS_JSONL")
        && !env_path.trim().is_empty()
    {
        return PathBuf::from(env_path);
    }

    // Priority 2: metadata.json override (if explicitly set to non-default)
    let metadata_jsonl = &metadata.jsonl_export;
    let is_explicit_override =
        metadata_jsonl != DEFAULT_JSONL_FILENAME && !is_excluded_jsonl(metadata_jsonl);

    if is_explicit_override {
        let candidate = PathBuf::from(metadata_jsonl);
        return if candidate.is_absolute() {
            candidate
        } else {
            beads_dir.join(candidate)
        };
    }

    // Priority 3: DB override uses a sibling JSONL file. Prefer an existing
    // issues.jsonl/beads.jsonl next to the overridden DB before falling back
    // to the default issues.jsonl path.
    if db_override.is_some() {
        return db_override.and_then(|path| path.parent()).map_or_else(
            || beads_dir.join(DEFAULT_JSONL_FILENAME),
            |parent| discover_jsonl(parent).unwrap_or_else(|| parent.join(DEFAULT_JSONL_FILENAME)),
        );
    }

    // Priority 4: File discovery (prefer issues.jsonl, fall back to beads.jsonl)
    if let Some(discovered) = discover_jsonl(beads_dir) {
        return discovered;
    }

    // Priority 5: Default (issues.jsonl) for writing when nothing exists
    beads_dir.join(DEFAULT_JSONL_FILENAME)
}

/// A configuration layer split into startup-only and runtime (DB) keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayer {
    pub startup: HashMap<String, String>,
    pub runtime: HashMap<String, String>,
}

impl ConfigLayer {
    /// Merge another layer on top of this one (higher precedence wins).
    ///
    /// Keys are normalized (hyphens replaced with underscores) before insertion
    /// so that `issue-prefix` (from YAML) and `issue_prefix` (from defaults)
    /// are treated as the same key and higher-precedence layers always win.
    pub fn merge_from(&mut self, other: &Self) {
        for (key, value) in &other.startup {
            let canonical = key.replace('-', "_");
            // Remove any variant of this key that already exists under a
            // different spelling (e.g. hyphenated vs underscored).
            if canonical == *key {
                let hyphenated = key.replace('_', "-");
                if hyphenated != *key {
                    self.startup.remove(&hyphenated);
                }
            } else {
                self.startup.remove(&canonical);
            }
            self.startup.insert(canonical, value.clone());
        }
        for (key, value) in &other.runtime {
            let canonical = key.replace('-', "_");
            if canonical == *key {
                let hyphenated = key.replace('_', "-");
                if hyphenated != *key {
                    self.runtime.remove(&hyphenated);
                }
            } else {
                self.runtime.remove(&canonical);
            }
            self.runtime.insert(canonical, value.clone());
        }
    }

    /// Merge multiple layers in precedence order (lowest to highest).
    #[must_use]
    pub fn merge_layers(layers: &[Self]) -> Self {
        let mut merged = Self::default();
        for layer in layers {
            merged.merge_from(layer);
        }
        merged
    }

    /// Build a layer from a YAML file path. Missing files return empty config.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn from_yaml(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)?;
        let value: serde_yml::Value = serde_yml::from_str(&contents)?;
        Ok(layer_from_yaml_value(&value))
    }

    /// Build a layer from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut layer = Self::default();

        for (key, value) in env::vars() {
            if let Some(stripped) = key.strip_prefix("BD_") {
                let normalized = stripped.to_lowercase();
                for variant in env_key_variants(&normalized) {
                    insert_key_value(&mut layer, &variant, value.clone());
                }
            }
        }

        if let Ok(value) = env::var("BEADS_FLUSH_DEBOUNCE") {
            insert_key_value(&mut layer, "flush-debounce", value);
        }
        if let Ok(value) = env::var("BEADS_IDENTITY") {
            insert_key_value(&mut layer, "identity", value);
        }
        if let Ok(value) = env::var("BEADS_REMOTE_SYNC_INTERVAL") {
            insert_key_value(&mut layer, "remote-sync-interval", value);
        }
        if let Ok(value) = env::var("BEADS_AUTO_START_DAEMON")
            && let Some(enabled) = parse_bool(&value)
        {
            insert_key_value(&mut layer, "no-daemon", (!enabled).to_string());
        }

        layer
    }

    /// Build a layer from DB config table values.
    ///
    /// # Errors
    ///
    /// Returns an error if config table lookup fails.
    pub fn from_db(storage: &SqliteStorage) -> Result<Self> {
        let mut layer = Self::default();
        let map = storage.get_all_config()?;
        for (key, value) in map {
            if is_startup_key(&key) {
                continue;
            }
            layer.runtime.insert(key, value);
        }
        Ok(layer)
    }
}

/// CLI overrides for config loading (optional).
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub db: Option<PathBuf>,
    pub actor: Option<String>,
    pub identity: Option<String>,
    pub json: Option<bool>,
    pub display_color: Option<bool>,
    pub quiet: Option<bool>,
    pub allow_stale: Option<bool>,
    pub no_db: Option<bool>,
    pub no_daemon: Option<bool>,
    pub no_auto_flush: Option<bool>,
    pub no_auto_import: Option<bool>,
    pub lock_timeout: Option<u64>,
}

impl CliOverrides {
    #[must_use]
    pub fn as_layer(&self) -> ConfigLayer {
        let mut layer = ConfigLayer::default();

        if let Some(path) = &self.db {
            insert_key_value(&mut layer, "db", path.to_string_lossy().to_string());
        }
        if let Some(actor) = &self.actor {
            insert_key_value(&mut layer, "actor", actor.clone());
        }
        if let Some(identity) = &self.identity {
            insert_key_value(&mut layer, "identity", identity.clone());
        }
        if let Some(json) = self.json {
            insert_key_value(&mut layer, "json", json.to_string());
        }
        if let Some(display_color) = self.display_color {
            insert_key_value(&mut layer, "display.color", display_color.to_string());
        }
        if let Some(no_db) = self.no_db {
            insert_key_value(&mut layer, "no-db", no_db.to_string());
        }
        if let Some(no_daemon) = self.no_daemon {
            insert_key_value(&mut layer, "no-daemon", no_daemon.to_string());
        }
        if let Some(no_auto_flush) = self.no_auto_flush {
            // Store as the canonical positive key so it wins over any legacy
            // `no-auto-flush` entry in the project/user config when merged.
            insert_key_value(&mut layer, "sync.auto_flush", (!no_auto_flush).to_string());
        }
        if let Some(no_auto_import) = self.no_auto_import {
            insert_key_value(
                &mut layer,
                "sync.auto_import",
                (!no_auto_import).to_string(),
            );
        }
        if let Some(lock_timeout) = self.lock_timeout {
            insert_key_value(&mut layer, "lock-timeout", lock_timeout.to_string());
        }

        layer
    }
}

/// Load project config (.beads/config.yaml).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_project_config(beads_dir: &Path) -> Result<ConfigLayer> {
    ConfigLayer::from_yaml(&beads_dir.join("config.yaml"))
}

/// Load user config (~/.config/beads/config.yaml), falling back to ~/.config/bd/config.yaml.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_user_config() -> Result<ConfigLayer> {
    let Ok(home) = env::var("HOME") else {
        return Ok(ConfigLayer::default());
    };
    let config_root = Path::new(&home).join(".config");
    let beads_path = config_root.join("beads").join("config.yaml");
    if beads_path.exists() {
        return ConfigLayer::from_yaml(&beads_path);
    }
    let legacy_path = config_root.join("bd").join("config.yaml");
    ConfigLayer::from_yaml(&legacy_path)
}

/// Load legacy user config (~/.beads/config.yaml).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_legacy_user_config() -> Result<ConfigLayer> {
    let Ok(home) = env::var("HOME") else {
        return Ok(ConfigLayer::default());
    };
    let path = Path::new(&home).join(".beads").join("config.yaml");
    ConfigLayer::from_yaml(&path)
}

/// Load startup-only configuration layers (YAML + env, no DB).
///
/// # Errors
///
/// Returns an error if any config file cannot be read or parsed.
pub fn load_startup_config(beads_dir: &Path) -> Result<ConfigLayer> {
    let legacy_user = load_legacy_user_config()?;
    let user = load_user_config()?;
    let project = load_project_config(beads_dir)?;
    let env_layer = ConfigLayer::from_env();

    Ok(ConfigLayer::merge_layers(&[
        legacy_user,
        user,
        project,
        env_layer,
    ]))
}

/// Default config layer (lowest precedence).
#[must_use]
pub fn default_config_layer() -> ConfigLayer {
    let mut layer = ConfigLayer::default();
    layer
        .runtime
        .insert("issue_prefix".to_string(), "br".to_string());
    layer
}

/// Load configuration with classic precedence order.
///
/// # Errors
///
/// Returns an error if any config file cannot be read or parsed, or DB access fails.
pub fn load_config(
    beads_dir: &Path,
    storage: Option<&SqliteStorage>,
    cli: &CliOverrides,
) -> Result<ConfigLayer> {
    let startup = load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    load_config_from_startup_layers(&startup.layers, &startup.paths.jsonl_path, storage, cli)
}

fn load_config_from_startup_layers(
    startup_layers: &[ConfigLayer],
    jsonl_path: &Path,
    storage: Option<&SqliteStorage>,
    cli: &CliOverrides,
) -> Result<ConfigLayer> {
    let defaults = default_config_layer();

    // Infer issue prefix from the first issue in JSONL so workspaces with
    // non-"bd" prefixes don't silently fall back to "bd" when the DB layer
    // is missing the stored prefix (e.g. after auto-rebuild).
    // Uses a fast single-line read (not full-file scan) since this runs on
    // every command.
    let mut jsonl_inferred = ConfigLayer::default();
    if let Some(prefix) = first_prefix_from_jsonl(jsonl_path)? {
        jsonl_inferred
            .runtime
            .insert("issue_prefix".to_string(), prefix);
    }

    let db_layer = match storage {
        Some(storage) => ConfigLayer::from_db(storage)?,
        None => ConfigLayer::default(),
    };
    let cli_layer = cli.as_layer();

    let mut layers = vec![defaults, jsonl_inferred, db_layer];
    layers.extend(startup_layers.iter().cloned());
    layers.push(cli_layer);

    Ok(ConfigLayer::merge_layers(&layers))
}

/// Internal structure to hold startup config and paths without redundant IO.
pub struct StartupConfig {
    pub paths: ConfigPaths,
    pub layers: Vec<ConfigLayer>,
    pub merged_config: ConfigLayer,
}

/// Load startup-only config layers and resolve the effective storage paths once.
///
/// # Errors
///
/// Returns an error if any startup config layer cannot be read or parsed, or if
/// path resolution fails.
pub fn load_startup_config_with_paths(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> Result<StartupConfig> {
    let legacy_user = load_legacy_user_config()?;
    let user = load_user_config()?;
    let project = load_project_config(beads_dir)?;
    let env_layer = ConfigLayer::from_env();

    let resolved_db_override = db_override.cloned().or_else(|| {
        [
            resolve_db_override_from_layer(beads_dir, &env_layer),
            resolve_db_override_from_layer(beads_dir, &project),
            resolve_db_override_from_layer(beads_dir, &user),
            resolve_db_override_from_layer(beads_dir, &legacy_user),
        ]
        .into_iter()
        .flatten()
        .next()
    });

    let layers = vec![legacy_user, user, project, env_layer];
    let merged_startup = ConfigLayer::merge_layers(&layers);

    let paths = ConfigPaths::resolve(beads_dir, resolved_db_override.as_ref())?;

    Ok(StartupConfig {
        paths,
        layers,
        merged_config: merged_startup,
    })
}

#[must_use]
pub(crate) fn configured_issue_prefix_from_map(
    config_map: &HashMap<String, String>,
) -> Option<String> {
    ["issue_prefix", "issue-prefix", "prefix"]
        .iter()
        .filter_map(|key| config_map.get(*key))
        .map(String::as_str)
        .map(str::trim)
        .find(|prefix| !prefix.is_empty())
        .map(str::to_string)
}

/// Build ID generation config from a merged config layer.
#[must_use]
pub fn id_config_from_layer(layer: &ConfigLayer) -> IdConfig {
    let prefix = get_value(layer, &["issue_prefix", "issue-prefix", "prefix"])
        .cloned()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "br".to_string());

    let min_hash_length = parse_usize(layer, &["min_hash_length", "min-hash-length"]).unwrap_or(3);
    let max_hash_length = parse_usize(layer, &["max_hash_length", "max-hash-length"]).unwrap_or(8);
    let max_collision_prob =
        parse_f64(layer, &["max_collision_prob", "max-collision-prob"]).unwrap_or(0.25);

    IdConfig {
        prefix,
        min_hash_length,
        max_hash_length,
        max_collision_prob,
    }
}

/// Resolve default priority for new issues from config.
///
/// # Errors
///
/// Returns an error if the configured value is not a valid priority (0-4).
pub fn default_priority_from_layer(layer: &ConfigLayer) -> Result<Priority> {
    get_value(layer, &["default_priority", "default-priority"])
        .map_or_else(|| Ok(Priority::MEDIUM), |value| Priority::from_str(value))
}

/// Resolve default issue type for new issues from config.
///
/// # Errors
///
/// Returns an error only if parsing fails (custom types are allowed).
pub fn default_issue_type_from_layer(layer: &ConfigLayer) -> Result<IssueType> {
    get_value(layer, &["default_type", "default-type"])
        .map_or_else(|| Ok(IssueType::Task), |value| IssueType::from_str(value))
}

/// Resolve display color preference from a merged config layer.
///
/// Accepts keys: `display.color`, `display-color`, `display_color`.
#[must_use]
pub fn display_color_from_layer(layer: &ConfigLayer) -> Option<bool> {
    get_value(layer, &["display.color", "display-color", "display_color"])
        .and_then(|value| parse_bool(value))
}

/// Determine whether human-readable output should use ANSI color.
///
/// Precedence:
/// 1) Config `display.color` (if set)
/// 3) `NO_COLOR` environment variable (standard)
/// 3) stdout is a terminal
#[must_use]
pub fn should_use_color(layer: &ConfigLayer) -> bool {
    if let Some(value) = display_color_from_layer(layer) {
        return value;
    }
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Resolve external project mappings from config.
///
/// Supports `external_projects.<name>` or `external-projects.<name>` keys.
/// Relative paths are resolved against the project root (parent of `.beads`).
#[must_use]
pub fn external_projects_from_layer(
    layer: &ConfigLayer,
    beads_dir: &Path,
) -> HashMap<String, PathBuf> {
    let base_dir = beads_dir.parent().unwrap_or(beads_dir);
    let mut map = HashMap::new();
    // Startup keys are lower precedence than runtime keys in merged config.
    // Insert startup first so runtime values win on duplicate project names.
    let iter = layer.startup.iter().chain(layer.runtime.iter());

    for (key, value) in iter {
        let key_lower = key.to_lowercase();
        let is_external = key_lower.starts_with("external_projects.")
            || key_lower.starts_with("external-projects.");
        if !is_external {
            continue;
        }

        let project = key.split_once('.').map(|(_, rest)| rest);
        let Some(project) = project.filter(|p| !p.trim().is_empty()) else {
            continue;
        };

        let path = PathBuf::from(value.trim());
        let resolved = if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        };
        map.insert(project.trim().to_string(), resolved);
    }

    map
}

/// Resolve external project DB paths from config.
///
/// Projects are expected to be either a `.beads` directory or a project root
/// containing `.beads/`.
#[must_use]
pub fn external_project_db_paths(
    layer: &ConfigLayer,
    beads_dir: &Path,
) -> HashMap<String, PathBuf> {
    let projects = external_projects_from_layer(layer, beads_dir);
    let mut db_paths = HashMap::new();

    for (name, path) in projects {
        let beads_path = if path.file_name().is_some_and(is_beads_dir_name) {
            path.clone()
        } else if path.join("_beads").is_dir() {
            path.join("_beads")
        } else {
            path.join(".beads")
        };

        if !beads_path.is_dir() {
            warn!(
                project = %name,
                path = %beads_path.display(),
                "External project .beads directory not found"
            );
            continue;
        }

        match ConfigPaths::resolve(&beads_path, None) {
            Ok(paths) => {
                db_paths.insert(name, paths.db_path);
            }
            Err(err) => {
                warn!(
                    project = %name,
                    path = %beads_path.display(),
                    error = %err,
                    "Failed to resolve external project DB path"
                );
            }
        }
    }

    db_paths
}

/// Resolve actor from a merged config layer.
#[must_use]
pub fn actor_from_layer(layer: &ConfigLayer) -> Option<String> {
    get_startup_value(layer, &["actor"])
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Resolve actor with fallback to USER and a safe default.
#[must_use]
pub fn resolve_actor(layer: &ConfigLayer) -> String {
    actor_from_layer(layer)
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Read the `claim-exclusive` config key.
///
/// When true, `--claim` rejects re-claims even by the same actor.
/// Accepts `claim.exclusive`, `claim_exclusive`, or `claim-exclusive`.
#[must_use]
pub fn claim_exclusive_from_layer(layer: &ConfigLayer) -> bool {
    get_startup_value(layer, &["claim-exclusive", "claim.exclusive"])
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

/// Determine if a key is startup-only.
///
/// Startup-only keys can only be set in YAML config files, not in the database.
/// These include path settings, behavior flags, and git-related options.
#[must_use]
pub fn is_startup_key(key: &str) -> bool {
    let normalized = normalize_key(key);

    if normalized.starts_with("git.")
        || normalized.starts_with("routing.")
        || normalized.starts_with("validation.")
        || normalized.starts_with("directory.")
        || normalized.starts_with("sync.")
        || normalized.starts_with("display.")
        || normalized.starts_with("external-projects.")
    {
        return true;
    }

    matches!(
        normalized.as_str(),
        "no-db"
            | "no-daemon"
            | "no-auto-flush"
            | "no-auto-import"
            | "json"
            | "db"
            | "actor"
            | "identity"
            | "flush-debounce"
            | "lock-timeout"
            | "remote-sync-interval"
            | "no-git-ops"
            | "no-push"
            | "sync-branch"
            | "sync.branch"
            | "external-projects"
            | "hierarchy.max-depth"
    )
}

fn insert_key_value(layer: &mut ConfigLayer, key: &str, value: String) {
    // Normalize hyphens to underscores so YAML keys like `issue-prefix`
    // are stored under the same canonical key as `issue_prefix`.
    let canonical = key.replace('-', "_");
    if is_startup_key(key) {
        layer.startup.insert(canonical, value);
    } else {
        layer.runtime.insert(canonical, value);
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase().replace('_', "-")
}

fn env_key_variants(raw: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let raw_lower = raw.to_lowercase();
    variants.push(raw_lower.clone());
    variants.push(raw_lower.replace('_', "."));
    variants.push(raw_lower.replace('_', "-"));
    variants
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn get_startup_value<'a>(layer: &'a ConfigLayer, keys: &[&str]) -> Option<&'a String> {
    let normalized_keys: Vec<String> = keys.iter().map(|key| normalize_key(key)).collect();
    for (key, value) in &layer.startup {
        let normalized = normalize_key(key);
        if normalized_keys
            .iter()
            .any(|candidate| candidate == &normalized)
        {
            return Some(value);
        }
    }
    None
}

fn get_value<'a>(layer: &'a ConfigLayer, keys: &[&str]) -> Option<&'a String> {
    let normalized_keys: Vec<String> = keys.iter().map(|key| normalize_key(key)).collect();
    for (key, value) in &layer.runtime {
        let normalized = normalize_key(key);
        if normalized_keys
            .iter()
            .any(|candidate| candidate == &normalized)
        {
            return Some(value);
        }
    }
    None
}

fn parse_usize(layer: &ConfigLayer, keys: &[&str]) -> Option<usize> {
    get_value(layer, keys).and_then(|value| value.trim().parse::<usize>().ok())
}

fn parse_f64(layer: &ConfigLayer, keys: &[&str]) -> Option<f64> {
    get_value(layer, keys).and_then(|value| value.trim().parse::<f64>().ok())
}

fn db_override_from_layer(layer: &ConfigLayer) -> Option<PathBuf> {
    get_startup_value(layer, &["db", "database"]).and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    })
}

fn resolve_db_override_from_layer(beads_dir: &Path, layer: &ConfigLayer) -> Option<PathBuf> {
    db_override_from_layer(layer).map(|path| {
        if path.is_absolute() {
            path
        } else {
            crate::util::resolve_cache_dir(beads_dir).join(path)
        }
    })
}

fn lock_timeout_from_layer(layer: &ConfigLayer) -> Option<u64> {
    get_startup_value(layer, &["lock-timeout", "lock_timeout"])
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn layer_from_yaml_value(value: &serde_yml::Value) -> ConfigLayer {
    let mut layer = ConfigLayer::default();
    let mut flat = HashMap::new();
    flatten_yaml(value, "", &mut flat);

    for (key, value) in flat {
        insert_key_value(&mut layer, &key, value);
    }

    layer
}

fn flatten_yaml(value: &serde_yml::Value, prefix: &str, out: &mut HashMap<String, String>) {
    match value {
        serde_yml::Value::Mapping(map) => {
            for (key, value) in map {
                let Some(key_str) = key.as_str() else {
                    continue;
                };
                let next_prefix = if prefix.is_empty() {
                    key_str.to_string()
                } else {
                    format!("{prefix}.{key_str}")
                };
                flatten_yaml(value, &next_prefix, out);
            }
        }
        serde_yml::Value::Sequence(values) => {
            let joined = values
                .iter()
                .filter_map(yaml_scalar_to_string)
                .collect::<Vec<_>>()
                .join(",");
            out.insert(prefix.to_string(), joined);
        }
        _ => {
            if let Some(value) = yaml_scalar_to_string(value) {
                out.insert(prefix.to_string(), value);
            }
        }
    }
}

fn yaml_scalar_to_string(value: &serde_yml::Value) -> Option<String> {
    match value {
        serde_yml::Value::Bool(v) => Some(v.to_string()),
        serde_yml::Value::Number(n) => Some(n.to_string()),
        serde_yml::Value::String(s) => Some(s.clone()),
        serde_yml::Value::Null | serde_yml::Value::Sequence(_) | serde_yml::Value::Mapping(_) => {
            None
        }
        serde_yml::Value::Tagged(tagged) => yaml_scalar_to_string(&tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use std::process::Command;
    use tempfile::TempDir;

    fn write_issue_jsonl(path: &Path, issue: &Issue) {
        let json = serde_json::to_string(&issue).expect("serialize issue");
        fs::write(path, format!("{json}\n")).expect("write jsonl");
    }

    fn write_single_issue_jsonl(path: &Path, id: &str, title: &str) {
        let now = Utc::now();
        let issue = Issue {
            id: id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        write_issue_jsonl(path, &issue);
    }

    fn create_malformed_blocked_cache_db(db_path: &Path) {
        let create = Command::new("sqlite3")
            .args([
                db_path.to_str().expect("db path utf8"),
                "CREATE TABLE blocked_issues_cache (issue_id TEXT PRIMARY KEY, blocked_by TEXT NOT NULL, blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP);",
            ])
            .output()
            .expect("run sqlite3 create");
        assert!(
            create.status.success(),
            "sqlite3 create failed: {}",
            String::from_utf8_lossy(&create.stderr)
        );

        let mutate_master = Command::new("sqlite3")
            .arg(db_path)
            .args([
                "-cmd",
                ".dbconfig defensive off",
                "-cmd",
                "PRAGMA writable_schema=ON;",
                "INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql) SELECT type,name,tbl_name,rootpage,sql FROM sqlite_master WHERE name='blocked_issues_cache';",
            ])
            .output()
            .expect("run sqlite3 writable_schema");
        assert!(
            mutate_master.status.success(),
            "sqlite3 writable_schema failed: {}",
            String::from_utf8_lossy(&mutate_master.stderr)
        );
    }

    fn insert_duplicate_issue_prefix_config_row(db_path: &Path, value: &str) {
        let insert = Command::new("sqlite3")
            .args([
                db_path.to_str().expect("db path utf8"),
                &format!(
                    "INSERT INTO config (key, value) VALUES ('issue_prefix', '{}');",
                    value.replace('\'', "''")
                ),
            ])
            .output()
            .expect("run sqlite3 duplicate config insert");
        assert!(
            insert.status.success(),
            "sqlite3 duplicate config insert failed: {}",
            String::from_utf8_lossy(&insert.stderr)
        );
    }

    #[test]
    fn metadata_defaults_when_missing() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(metadata.database, DEFAULT_DB_FILENAME);
        assert_eq!(metadata.jsonl_export, DEFAULT_JSONL_FILENAME);
    }

    #[test]
    fn metadata_override_paths() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "custom.db", "jsonl_export": "custom.jsonl"}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");
        assert_eq!(paths.db_path, beads_dir.join("custom.db"));
        assert_eq!(paths.jsonl_path, beads_dir.join("custom.jsonl"));
    }

    #[test]
    fn merge_precedence_order() {
        let mut defaults = default_config_layer();
        defaults
            .runtime
            .insert("issue_prefix".to_string(), "bd".to_string());

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db".to_string());

        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml".to_string());

        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env".to_string());

        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("issue_prefix".to_string(), "cli".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "cli");
    }

    #[test]
    fn yaml_startup_keys_are_separated() {
        let yaml = r"
no-db: true
issue_prefix: bd
";
        let value: serde_yml::Value = serde_yml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);
        assert_eq!(layer.startup.get("no_db").unwrap(), "true");
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "bd");
    }

    #[test]
    fn yaml_sequence_flattens_to_csv() {
        let yaml = r"
labels:
  - backend
  - api
";
        let value: serde_yml::Value = serde_yml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);
        assert_eq!(layer.runtime.get("labels").unwrap(), "backend,api");
    }

    #[test]
    fn id_config_parses_numeric_overrides() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("issue_prefix".to_string(), "br".to_string());
        layer
            .runtime
            .insert("min_hash_length".to_string(), "4".to_string());
        layer
            .runtime
            .insert("max_hash_length".to_string(), "10".to_string());
        layer
            .runtime
            .insert("max_collision_prob".to_string(), "0.5".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "br");
        assert_eq!(config.min_hash_length, 4);
        assert_eq!(config.max_hash_length, 10);
        assert!((config.max_collision_prob - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn default_priority_from_layer_uses_config_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_priority".to_string(), "1".to_string());

        let priority = default_priority_from_layer(&layer).expect("default priority");
        assert_eq!(priority, Priority::HIGH);
    }

    #[test]
    fn default_priority_from_layer_errors_on_invalid_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_priority".to_string(), "9".to_string());

        assert!(default_priority_from_layer(&layer).is_err());
    }

    #[test]
    fn default_issue_type_from_layer_uses_config_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_type".to_string(), "feature".to_string());

        let issue_type = default_issue_type_from_layer(&layer).expect("default type");
        assert_eq!(issue_type, IssueType::Feature);
    }

    #[test]
    fn db_layer_skips_startup_keys() {
        let mut storage = SqliteStorage::open_memory().expect("storage");
        storage.set_config("no-db", "true").expect("set no-db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("set issue_prefix");

        let layer = ConfigLayer::from_db(&storage).expect("db layer");
        assert!(!layer.startup.contains_key("no_db"));
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "bd");
    }

    #[test]
    fn startup_layer_reads_db_override() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("db".to_string(), "/tmp/beads.db".to_string());

        let override_path = db_override_from_layer(&layer).expect("db override");
        assert_eq!(override_path, PathBuf::from("/tmp/beads.db"));
    }

    #[test]
    fn resolve_db_override_from_layer_anchors_relative_paths_to_beads_cache_dir() {
        let beads_dir = PathBuf::from("/tmp/project/.beads");
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("db".to_string(), "custom.db".to_string());

        let override_path =
            resolve_db_override_from_layer(&beads_dir, &layer).expect("db override");
        assert_eq!(
            override_path,
            crate::util::resolve_cache_dir(&beads_dir).join("custom.db")
        );
    }

    #[test]
    fn startup_layer_reads_lock_timeout() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("lock_timeout".to_string(), "2500".to_string());

        let timeout = lock_timeout_from_layer(&layer).expect("lock timeout");
        assert_eq!(timeout, 2500);
    }

    // ==================== Additional Config Unit Tests ====================
    // Tests for beads_rust-7h9: Config unit tests - Layered configuration

    #[test]
    fn precedence_default_is_lowest() {
        // Verify that default layer values are overridden by any other layer
        let defaults = default_config_layer();
        assert_eq!(defaults.runtime.get("issue_prefix").unwrap(), "br");

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "from_db".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "from_db");
    }

    #[test]
    fn precedence_db_overrides_default() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "db_prefix");
    }

    #[test]
    fn precedence_yaml_overrides_db() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db_prefix".to_string());
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "yaml_prefix");
    }

    #[test]
    fn precedence_env_overrides_yaml() {
        let defaults = default_config_layer();
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml_prefix".to_string());
        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, yaml, env_layer]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "env_prefix");
    }

    #[test]
    fn precedence_cli_overrides_all() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db".to_string());
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml".to_string());
        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env".to_string());
        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("issue_prefix".to_string(), "cli_wins".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "cli_wins");
    }

    #[test]
    fn precedence_chain_includes_legacy_and_user_layers() {
        let defaults = default_config_layer();

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db".to_string());

        let mut legacy = ConfigLayer::default();
        legacy
            .runtime
            .insert("issue_prefix".to_string(), "legacy".to_string());

        let mut user = ConfigLayer::default();
        user.runtime
            .insert("issue_prefix".to_string(), "user".to_string());

        let mut project = ConfigLayer::default();
        project
            .runtime
            .insert("issue_prefix".to_string(), "project".to_string());

        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env".to_string());

        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("issue_prefix".to_string(), "cli".to_string());

        let merged =
            ConfigLayer::merge_layers(&[defaults, db, legacy, user, project, env_layer, cli]);

        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "cli");
    }

    #[test]
    fn precedence_full_chain_with_different_keys() {
        // Each layer sets a different key, all should be preserved
        let mut defaults = default_config_layer();
        defaults
            .runtime
            .insert("from_default".to_string(), "default_value".to_string());

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("from_db".to_string(), "db_value".to_string());

        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("from_yaml".to_string(), "yaml_value".to_string());

        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("from_env".to_string(), "env_value".to_string());

        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("from_cli".to_string(), "cli_value".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);

        assert_eq!(merged.runtime.get("from_default").unwrap(), "default_value");
        assert_eq!(merged.runtime.get("from_db").unwrap(), "db_value");
        assert_eq!(merged.runtime.get("from_yaml").unwrap(), "yaml_value");
        assert_eq!(merged.runtime.get("from_env").unwrap(), "env_value");
        assert_eq!(merged.runtime.get("from_cli").unwrap(), "cli_value");
    }

    #[test]
    fn metadata_handles_empty_strings() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with empty strings
        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "", "jsonl_export": "  "}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        // Empty strings should fall back to defaults
        assert_eq!(loaded.database, DEFAULT_DB_FILENAME);
        assert_eq!(loaded.jsonl_export, DEFAULT_JSONL_FILENAME);
    }

    #[test]
    fn metadata_handles_extra_fields() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with extra fields (should be ignored)
        let metadata_path = beads_dir.join("metadata.json");
        let metadata =
            r#"{"database": "test.db", "jsonl_export": "test.jsonl", "unknown_field": true}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.database, "test.db");
        assert_eq!(loaded.jsonl_export, "test.jsonl");
    }

    #[test]
    fn metadata_with_backend_and_retention() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "beads.db", "jsonl_export": "issues.jsonl", "backend": "sqlite", "deletions_retention_days": 30}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.backend, Some("sqlite".to_string()));
        assert_eq!(loaded.deletions_retention_days, Some(30));
    }

    #[test]
    fn discover_beads_dir_returns_error_when_not_found() {
        let temp = TempDir::new().expect("tempdir");
        // No .beads directory created

        let result = discover_beads_dir(Some(temp.path()));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BeadsError::NotInitialized));
    }

    #[test]
    fn discover_beads_dir_finds_at_root() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let discovered = discover_beads_dir(Some(temp.path())).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_deeply_nested() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create deeply nested directory
        let nested = temp
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("d")
            .join("e");
        fs::create_dir_all(&nested).expect("create nested");

        let discovered = discover_beads_dir(Some(&nested)).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_optional_beads_dir_with_cli_uses_explicit_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join("external").join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let cli = CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            ..CliOverrides::default()
        };

        let discovered =
            discover_optional_beads_dir_with_cli(&cli).expect("optional discovery with db");
        assert_eq!(discovered, Some(beads_dir));
    }

    #[test]
    fn discover_beads_dir_with_cli_from_uses_env_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join("external").join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let discovered =
            discover_beads_dir_with_cli_from(None, &CliOverrides::default(), None, Some(&db_path))
                .expect("discovery with env db override");

        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_optional_beads_dir_with_cli_follows_redirect_for_explicit_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let source_beads = temp.path().join("source").join(".beads");
        let target_beads = temp.path().join("target").join(".beads");
        fs::create_dir_all(&source_beads).expect("create source beads dir");
        fs::create_dir_all(&target_beads).expect("create target beads dir");
        fs::write(source_beads.join("redirect"), "../../target/.beads").expect("write redirect");

        let cli = CliOverrides {
            db: Some(source_beads.join("beads.db")),
            ..CliOverrides::default()
        };

        let discovered =
            discover_optional_beads_dir_with_cli(&cli).expect("optional discovery with redirect");
        assert_eq!(discovered, Some(target_beads));
    }

    #[test]
    fn discover_beads_dir_with_env_override_rejects_invalid_path_even_when_workspace_exists() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let invalid = temp.path().join("missing").join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let err = discover_beads_dir_with_env(Some(temp.path()), Some(&invalid))
            .expect_err("invalid override should fail");
        assert!(matches!(err, BeadsError::Config(_)));
        assert!(
            err.to_string()
                .contains("not found or not a .beads directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn discover_beads_dir_with_cli_from_falls_back_to_workspace_for_external_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let discovered = discover_beads_dir_with_cli_from(
            Some(&start),
            &CliOverrides::default(),
            None,
            Some(Path::new("/tmp/not-a-beads-db")),
        )
        .expect("external env db should still reuse discovered workspace");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_with_cli_from_falls_back_to_workspace_for_external_cli_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let discovered = discover_beads_dir_with_cli_from(
            Some(&start),
            &CliOverrides {
                db: Some(temp.path().join("cache").join("custom.db")),
                ..CliOverrides::default()
            },
            None,
            None,
        )
        .expect("external cli db override should reuse discovered workspace");

        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_with_cli_from_falls_back_to_workspace_for_relative_cli_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let discovered = discover_beads_dir_with_cli_from(
            Some(&start),
            &CliOverrides {
                db: Some(PathBuf::from("custom.db")),
                ..CliOverrides::default()
            },
            None,
            None,
        )
        .expect("relative cli db override should reuse discovered workspace");

        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_with_cli_from_errors_for_external_cli_db_override_without_workspace() {
        let temp = TempDir::new().expect("tempdir");
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let db_override = temp.path().join("cache").join("custom.db");
        let err = discover_beads_dir_with_cli_from(
            Some(&start),
            &CliOverrides {
                db: Some(db_override.clone()),
                ..CliOverrides::default()
            },
            None,
            None,
        )
        .expect_err("external cli db without workspace should error");

        assert!(matches!(err, BeadsError::WithContext { .. }));
        assert!(
            err.to_string()
                .contains(db_override.to_string_lossy().as_ref())
                && (err.to_string().contains("BEADS_DIR") || err.to_string().contains("workspace")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn discover_beads_dir_with_cli_from_errors_for_external_db_override_without_workspace() {
        let temp = TempDir::new().expect("tempdir");
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let err = discover_beads_dir_with_cli_from(
            Some(&start),
            &CliOverrides::default(),
            None,
            Some(Path::new("/tmp/not-a-beads-db")),
        )
        .expect_err("external env db without workspace should error");
        assert!(matches!(err, BeadsError::WithContext { .. }));
        assert!(
            err.to_string().contains("BEADS_DIR") || err.to_string().contains("workspace"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn env_key_variants_generates_all_forms() {
        let variants = env_key_variants("no_auto_flush");
        assert!(variants.contains(&"no_auto_flush".to_string()));
        assert!(variants.contains(&"no.auto.flush".to_string()));
        assert!(variants.contains(&"no-auto-flush".to_string()));
    }

    #[test]
    fn normalize_key_handles_various_formats() {
        assert_eq!(normalize_key("ISSUE_PREFIX"), "issue-prefix");
        assert_eq!(normalize_key("issue-prefix"), "issue-prefix");
        assert_eq!(normalize_key("issue_prefix"), "issue-prefix");
        assert_eq!(normalize_key("  ISSUE_PREFIX  "), "issue-prefix");
    }

    #[test]
    fn parse_bool_handles_all_truthy_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("yes"), Some(true));
        assert_eq!(parse_bool("YES"), Some(true));
        assert_eq!(parse_bool("y"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
    }

    #[test]
    fn parse_bool_handles_all_falsy_values() {
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("NO"), Some(false));
        assert_eq!(parse_bool("n"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
    }

    #[test]
    fn parse_bool_returns_none_for_invalid() {
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
        assert_eq!(parse_bool("2"), None);
    }

    #[test]
    fn is_startup_key_identifies_startup_keys() {
        assert!(is_startup_key("no-db"));
        assert!(is_startup_key("no-daemon"));
        assert!(is_startup_key("no-auto-flush"));
        assert!(is_startup_key("no-auto-import"));
        assert!(is_startup_key("json"));
        assert!(is_startup_key("db"));
        assert!(is_startup_key("actor"));
        assert!(is_startup_key("identity"));
        assert!(is_startup_key("lock-timeout"));
        assert!(is_startup_key("git.branch")); // prefix check
        assert!(is_startup_key("routing.policy")); // prefix check
    }

    #[test]
    fn is_startup_key_identifies_runtime_keys() {
        assert!(!is_startup_key("issue_prefix"));
        assert!(!is_startup_key("issue-prefix"));
        assert!(!is_startup_key("min_hash_length"));
        assert!(!is_startup_key("labels"));
    }

    #[test]
    fn resolve_db_path_absolute_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let absolute_path = "/absolute/path/to/beads.db";
        let metadata = Metadata {
            database: absolute_path.to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_db_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, PathBuf::from(absolute_path));
    }

    #[test]
    fn resolve_db_path_relative_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata {
            database: "relative.db".to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_db_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, beads_dir.join("relative.db"));
    }

    #[test]
    fn resolve_db_path_override_wins() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::default();
        let override_path = PathBuf::from("/override/path.db");

        let resolved = resolve_db_path(&beads_dir, &metadata, Some(&override_path));
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn resolve_jsonl_path_absolute_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let absolute_path = "/absolute/path/to/issues.jsonl";
        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: absolute_path.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, PathBuf::from(absolute_path));
    }

    #[test]
    fn resolve_jsonl_path_relative_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "relative.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, beads_dir.join("relative.jsonl"));
    }

    #[test]
    fn resolve_jsonl_path_db_override_derives_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::default();
        let db_override = PathBuf::from("/some/path/custom.db");

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, Some(&db_override));
        assert_eq!(resolved, PathBuf::from("/some/path/issues.jsonl"));
    }

    #[test]
    fn resolve_jsonl_path_db_override_prefers_existing_legacy_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let db_override = beads_dir.join("custom.db");
        fs::write(beads_dir.join("beads.jsonl"), "{}\n").expect("write legacy jsonl");

        let resolved = resolve_jsonl_path(&beads_dir, &Metadata::default(), Some(&db_override));
        assert_eq!(resolved, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn cli_overrides_as_layer_sets_startup_keys() {
        let cli = CliOverrides {
            db: Some(PathBuf::from("/cli/path.db")),
            actor: Some("cli_actor".to_string()),
            json: Some(true),
            display_color: None,
            quiet: None,
            allow_stale: None,
            no_db: Some(true),
            no_daemon: Some(true),
            no_auto_flush: Some(true),
            no_auto_import: Some(true),
            lock_timeout: Some(5000),
            identity: None,
        };

        let layer = cli.as_layer();

        assert_eq!(layer.startup.get("db").unwrap(), "/cli/path.db");
        assert_eq!(layer.startup.get("actor").unwrap(), "cli_actor");
        assert_eq!(layer.startup.get("json").unwrap(), "true");
        assert_eq!(layer.startup.get("no_db").unwrap(), "true");
        assert_eq!(layer.startup.get("no_daemon").unwrap(), "true");
        // no_auto_flush=true => sync.auto_flush=false (canonical positive key, inverted)
        assert_eq!(layer.startup.get("sync.auto_flush").unwrap(), "false");
        assert_eq!(layer.startup.get("sync.auto_import").unwrap(), "false");
        assert_eq!(layer.startup.get("lock_timeout").unwrap(), "5000");
    }

    #[test]
    fn cli_overrides_empty_produces_empty_layer() {
        let cli = CliOverrides::default();
        let layer = cli.as_layer();

        assert!(layer.startup.is_empty());
        assert!(layer.runtime.is_empty());
    }

    #[test]
    fn no_auto_flush_from_layer_reads_sync_auto_flush_false() {
        // A project config with `sync.auto_flush: false` should disable auto-flush.
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.auto_flush", "false".to_string());

        let result = no_auto_flush_from_layer(&layer);
        assert_eq!(
            result,
            Some(true),
            "sync.auto_flush=false should set no_auto_flush=true"
        );
    }

    #[test]
    fn no_auto_flush_from_layer_reads_sync_auto_flush_true() {
        // A project config with `sync.auto_flush: true` should enable auto-flush.
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.auto_flush", "true".to_string());

        let result = no_auto_flush_from_layer(&layer);
        assert_eq!(
            result,
            Some(false),
            "sync.auto_flush=true should set no_auto_flush=false"
        );
    }

    #[test]
    fn no_auto_flush_from_layer_canonical_key_beats_legacy_key() {
        // When both canonical `sync.auto_flush` and legacy `no-auto-flush` are present,
        // `sync.auto_flush` should take precedence.
        let mut layer = ConfigLayer::default();
        // sync.auto_flush=true means "enable auto-flush" (no_auto_flush=false)
        insert_key_value(&mut layer, "sync.auto_flush", "true".to_string());
        // no-auto-flush=true means "disable auto-flush" (no_auto_flush=true)
        insert_key_value(&mut layer, "no-auto-flush", "true".to_string());

        // sync.auto_flush should win -> no_auto_flush=false
        let result = no_auto_flush_from_layer(&layer);
        assert_eq!(
            result,
            Some(false),
            "sync.auto_flush=true should win over legacy no-auto-flush=true"
        );
    }

    #[test]
    fn no_auto_flush_from_layer_hyphen_underscore_equivalence() {
        // Config key `sync.auto-flush` (with hyphen) should be equivalent to
        // `sync.auto_flush` (with underscore).
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.auto-flush", "false".to_string());

        let result = no_auto_flush_from_layer(&layer);
        assert_eq!(
            result,
            Some(true),
            "sync.auto-flush=false (hyphen) should set no_auto_flush=true"
        );
    }

    #[test]
    fn cli_no_auto_flush_overrides_config_sync_auto_flush() {
        // When --no-auto-flush is passed on CLI (no_auto_flush=Some(true)),
        // it should translate to sync.auto_flush=false in the layer, overriding
        // any YAML config that has sync.auto_flush=true.
        let overrides = CliOverrides {
            no_auto_flush: Some(true),
            ..CliOverrides::default()
        };
        let cli_layer = overrides.as_layer();

        // Simulate a project config layer with sync.auto_flush=true
        let mut project_layer = ConfigLayer::default();
        insert_key_value(&mut project_layer, "sync.auto_flush", "true".to_string());

        // Merge: project first (lower precedence), CLI second (higher precedence)
        let merged = ConfigLayer::merge_layers(&[project_layer, cli_layer]);

        let result = no_auto_flush_from_layer(&merged);
        assert_eq!(
            result,
            Some(true),
            "CLI --no-auto-flush should override project config sync.auto_flush=true"
        );
    }

    #[test]
    fn yaml_nested_keys_flatten_with_dots() {
        let yaml = r"
sync:
  branch: main
git:
  auto_commit: true
routing:
  policy: fifo
";
        let value: serde_yml::Value = serde_yml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);

        // git.* and routing.* prefixes go to startup (per is_startup_key)
        // sync.branch is an explicit startup key
        assert!(layer.startup.contains_key("sync.branch"));
        assert!(layer.startup.contains_key("git.auto_commit"));
        assert!(layer.startup.contains_key("routing.policy"));
    }

    #[test]
    fn actor_from_layer_returns_none_for_empty() {
        let layer = ConfigLayer::default();
        assert!(actor_from_layer(&layer).is_none());

        let mut layer_with_empty = ConfigLayer::default();
        layer_with_empty
            .startup
            .insert("actor".to_string(), "   ".to_string());
        assert!(actor_from_layer(&layer_with_empty).is_none());
    }

    #[test]
    fn actor_from_layer_returns_trimmed_value() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("actor".to_string(), "  test_actor  ".to_string());

        let actor = actor_from_layer(&layer).expect("actor");
        assert_eq!(actor, "test_actor");
    }

    #[test]
    fn external_projects_runtime_mapping_overrides_startup_mapping() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let mut layer = ConfigLayer::default();
        layer.startup.insert(
            "external_projects.shared".to_string(),
            "startup-path".to_string(),
        );
        layer.runtime.insert(
            "external_projects.shared".to_string(),
            "runtime-path".to_string(),
        );

        let projects = external_projects_from_layer(&layer, &beads_dir);
        assert_eq!(
            projects.get("shared"),
            Some(&temp.path().join("runtime-path")),
            "Runtime config should override lower-precedence startup config"
        );
    }

    #[test]
    fn resolved_jsonl_path_drives_prefix_inference() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"custom.jsonl"}"#,
        )
        .expect("write metadata");
        write_single_issue_jsonl(&beads_dir.join("custom.jsonl"), "br-abc123", "custom issue");

        let paths = resolve_paths(&beads_dir, None).expect("resolve paths");
        assert_eq!(
            paths.jsonl_path,
            beads_dir.join("custom.jsonl"),
            "Metadata override should determine the active JSONL path"
        );
        assert_eq!(
            first_prefix_from_jsonl(&paths.jsonl_path).expect("infer prefix"),
            Some("br".to_string()),
            "Prefix inference should read from the resolved JSONL path"
        );
    }

    #[test]
    fn first_prefix_from_jsonl_preserves_hyphenated_prefixes() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let jsonl_path = beads_dir.join("issues.jsonl");

        write_single_issue_jsonl(
            &jsonl_path,
            "document-intelligence-0sa",
            "hyphenated prefix",
        );

        assert_eq!(
            first_prefix_from_jsonl(&jsonl_path).expect("infer prefix"),
            Some("document-intelligence".to_string())
        );
    }

    #[test]
    fn first_prefix_from_jsonl_preserves_hyphenated_prefixes_across_multiple_rows() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let jsonl_path = beads_dir.join("issues.jsonl");

        let first = serde_json::json!({
            "id": "document-intelligence-0sa",
            "title": "first",
            "status": "open",
        });
        let second = serde_json::json!({
            "id": "document-intelligence-1ab",
            "title": "second",
            "status": "open",
        });
        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).expect("serialize first"),
                serde_json::to_string(&second).expect("serialize second")
            ),
        )
        .expect("write jsonl");

        assert_eq!(
            first_prefix_from_jsonl(&jsonl_path).expect("infer prefix"),
            Some("document-intelligence".to_string())
        );
    }

    #[test]
    fn resolve_paths_honors_relative_project_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(beads_dir.join("config.yaml"), "db: custom.db\n").expect("write config");

        let paths = resolve_paths(&beads_dir, None).expect("resolve paths");
        assert_eq!(
            paths.db_path,
            crate::util::resolve_cache_dir(&beads_dir).join("custom.db")
        );
    }

    #[test]
    fn resolve_actor_falls_back_to_unknown() {
        let layer = ConfigLayer::default();
        // This test assumes USER env var may not be set in test context
        // or we need to verify the fallback mechanism
        let actor = resolve_actor(&layer);
        // Should be either USER env value or "unknown"
        assert!(!actor.is_empty());
    }

    #[test]
    fn merge_from_overwrites_existing_keys() {
        let mut base = ConfigLayer::default();
        base.runtime
            .insert("key1".to_string(), "base_value".to_string());
        base.startup
            .insert("key2".to_string(), "base_startup".to_string());

        let mut override_layer = ConfigLayer::default();
        override_layer
            .runtime
            .insert("key1".to_string(), "override_value".to_string());
        override_layer
            .startup
            .insert("key2".to_string(), "override_startup".to_string());

        base.merge_from(&override_layer);

        assert_eq!(base.runtime.get("key1").unwrap(), "override_value");
        assert_eq!(base.startup.get("key2").unwrap(), "override_startup");
    }

    #[test]
    fn merge_from_preserves_non_conflicting_keys() {
        let mut base = ConfigLayer::default();
        base.runtime
            .insert("base_only".to_string(), "base_value".to_string());

        let mut override_layer = ConfigLayer::default();
        override_layer
            .runtime
            .insert("override_only".to_string(), "override_value".to_string());

        base.merge_from(&override_layer);

        assert_eq!(base.runtime.get("base_only").unwrap(), "base_value");
        assert_eq!(base.runtime.get("override_only").unwrap(), "override_value");
    }

    #[test]
    fn config_paths_resolve_with_default_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        assert_eq!(paths.beads_dir, beads_dir);
        assert_eq!(paths.db_path, beads_dir.join(DEFAULT_DB_FILENAME));
        assert_eq!(paths.jsonl_path, beads_dir.join(DEFAULT_JSONL_FILENAME));
        assert_eq!(paths.metadata, Metadata::default());
    }

    #[test]
    fn load_project_config_returns_empty_when_missing() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let layer = load_project_config(&beads_dir).expect("project config");
        assert!(layer.startup.is_empty());
        assert!(layer.runtime.is_empty());
    }

    #[test]
    fn load_project_config_parses_yaml() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(
            beads_dir.join("config.yaml"),
            "issue_prefix: proj\nno-db: false\n",
        )
        .expect("write config");

        let layer = load_project_config(&beads_dir).expect("project config");
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "proj");
        assert_eq!(layer.startup.get("no_db").unwrap(), "false");
    }

    #[test]
    fn id_config_uses_defaults_when_keys_missing() {
        let layer = ConfigLayer::default();
        let config = id_config_from_layer(&layer);

        assert_eq!(config.prefix, "br");
        assert_eq!(config.min_hash_length, 3);
        assert_eq!(config.max_hash_length, 8);
        assert!((config.max_collision_prob - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn id_config_handles_hyphenated_keys() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("issue-prefix".to_string(), "hyphen".to_string());
        layer
            .runtime
            .insert("min-hash-length".to_string(), "5".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "hyphen");
        assert_eq!(config.min_hash_length, 5);
    }

    #[test]
    fn id_config_accepts_legacy_prefix_key() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("prefix".to_string(), "legacy".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "legacy");
    }

    // ==================== JSONL Discovery Tests ====================
    // Tests for beads_rust-ndl: JSONL discovery + metadata.json handling

    #[test]
    fn discover_jsonl_prefers_issues_over_legacy() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create both files
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let discovered = discover_jsonl(&beads_dir).expect("should discover");
        assert_eq!(discovered, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn discover_jsonl_falls_back_to_legacy() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let discovered = discover_jsonl(&beads_dir).expect("should discover");
        assert_eq!(discovered, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn discover_jsonl_returns_none_when_empty() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // No JSONL files
        let discovered = discover_jsonl(&beads_dir);
        assert!(discovered.is_none());
    }

    #[test]
    fn discover_jsonl_ignores_merge_artifacts() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only merge artifacts exist (should not be discovered)
        fs::write(beads_dir.join("beads.base.jsonl"), "{}").expect("write base");
        fs::write(beads_dir.join("beads.left.jsonl"), "{}").expect("write left");
        fs::write(beads_dir.join("beads.right.jsonl"), "{}").expect("write right");

        let discovered = discover_jsonl(&beads_dir);
        assert!(discovered.is_none());
    }

    #[test]
    fn is_excluded_jsonl_detects_merge_artifacts() {
        assert!(is_excluded_jsonl("beads.base.jsonl"));
        assert!(is_excluded_jsonl("beads.left.jsonl"));
        assert!(is_excluded_jsonl("beads.right.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_detects_deletion_log() {
        assert!(is_excluded_jsonl("deletions.jsonl"));
        assert!(is_excluded_jsonl("./deletions.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_detects_interaction_log() {
        assert!(is_excluded_jsonl("interactions.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_detects_excluded_basename_in_absolute_path() {
        assert!(is_excluded_jsonl("/tmp/beads.base.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_allows_valid_files() {
        assert!(!is_excluded_jsonl("issues.jsonl"));
        assert!(!is_excluded_jsonl("beads.jsonl"));
        assert!(!is_excluded_jsonl("custom.jsonl"));
    }

    #[test]
    fn resolve_jsonl_uses_discovery_when_no_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let metadata = Metadata::default();
        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);

        // Should discover beads.jsonl since issues.jsonl doesn't exist
        assert_eq!(resolved, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn resolve_jsonl_prefers_metadata_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Both legacy and custom exist
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");
        fs::write(beads_dir.join("custom.jsonl"), "{}").expect("write custom");

        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "custom.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        // Metadata override should win over discovered legacy/default filenames.
        assert_eq!(resolved, beads_dir.join("custom.jsonl"));
    }

    #[test]
    fn resolve_jsonl_ignores_excluded_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create issues.jsonl
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");

        // Metadata points to excluded file (should be ignored)
        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "deletions.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        // Should fall through to discovery, find issues.jsonl
        assert_eq!(resolved, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn resolve_jsonl_defaults_when_nothing_exists() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // No JSONL files exist
        let metadata = Metadata::default();
        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);

        // Should return default for writing
        assert_eq!(resolved, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn resolve_jsonl_db_override_derives_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let custom_dir = temp.path().join("custom");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&custom_dir).expect("create custom dir");

        // Create files in beads_dir (should be ignored)
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let metadata = Metadata::default();
        let db_override = custom_dir.join("custom.db");

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, Some(&db_override));
        // Should derive sibling from db_override path
        assert_eq!(resolved, custom_dir.join("issues.jsonl"));
    }

    #[test]
    fn config_paths_uses_discovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        // Should discover beads.jsonl
        assert_eq!(paths.jsonl_path, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn metadata_jsonl_override_respected() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with custom jsonl_export
        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database": "beads.db", "jsonl_export": "my-export.jsonl"}"#,
        )
        .expect("write metadata");

        // Create the custom file
        fs::write(beads_dir.join("my-export.jsonl"), "{}").expect("write custom");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");
        assert_eq!(paths.jsonl_path, beads_dir.join("my-export.jsonl"));
    }

    #[test]
    fn metadata_jsonl_override_respected_even_with_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with custom jsonl_export
        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database": "beads.db", "jsonl_export": "custom-name.jsonl"}"#,
        )
        .expect("write metadata");

        let db_override = beads_dir.join("beads.db");
        let metadata = Metadata::load(&beads_dir).expect("metadata");
        let resolved = resolve_jsonl_path(&beads_dir, &metadata, Some(&db_override));

        // Metadata override should still win when the database path is explicit.
        assert_eq!(
            resolved,
            beads_dir.join("custom-name.jsonl"),
            "Metadata should win over default sibling derivation when DB override is used"
        );
    }

    #[test]
    fn multiple_jsonl_candidates_prefers_issues() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create multiple candidates
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");
        fs::write(beads_dir.join("beads.base.jsonl"), "{}").expect("write base");
        fs::write(beads_dir.join("deletions.jsonl"), "{}").expect("write deletions");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        // Should pick issues.jsonl (preferred over legacy, ignoring excluded)
        assert_eq!(paths.jsonl_path, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn should_attempt_jsonl_recovery_only_for_corruption_errors() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&db_path, b"sqlite bytes").expect("write db placeholder");
        fs::write(&jsonl_path, "{}\n").expect("write jsonl");

        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::DatabaseCorrupt {
                detail: "bad page".to_string()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::NotADatabase {
                path: db_path.clone()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::WalCorrupt {
                detail: "bad wal".to_string()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::ShortRead {
                expected: 4096,
                actual: 12
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::TableExists {
                name: "blocked_issues_cache".to_string()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::IndexExists {
                name: "idx_blocked_cache_blocked_at".to_string()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::Internal(
                "malformed database schema (blocked_issues_cache) - table \"blocked_issues_cache\" already exists"
                    .to_string()
            )),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::Internal(
                "database disk image is malformed".to_string()
            )),
            &db_path,
            &jsonl_path
        ));
        assert!(should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::Internal(
                "row 13 missing from index idx_issues_list_active_order".to_string()
            )),
            &db_path,
            &jsonl_path
        ));

        assert!(!should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::SchemaChanged),
            &db_path,
            &jsonl_path
        ));
        assert!(!should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::CannotOpen {
                path: db_path.clone()
            }),
            &db_path,
            &jsonl_path
        ));
        assert!(!should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::Busy),
            &db_path,
            &jsonl_path
        ));
        assert!(!should_attempt_jsonl_recovery(
            &BeadsError::Database(FrankenError::Internal(
                "constraint verification failed".to_string()
            )),
            &db_path,
            &jsonl_path
        ));
    }

    #[test]
    fn resolve_bootstrap_issue_prefix_prefers_bootstrap_layer() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&jsonl_path, "").expect("write empty jsonl");

        let mut bootstrap_layer = ConfigLayer::default();
        bootstrap_layer
            .runtime
            .insert("issue_prefix".to_string(), "cfg".to_string());

        let prefix = resolve_bootstrap_issue_prefix(&bootstrap_layer, &beads_dir, &jsonl_path)
            .expect("prefix");
        assert_eq!(prefix, "cfg");
    }

    #[test]
    fn resolve_bootstrap_issue_prefix_abbreviates_long_dir_name() {
        let temp = TempDir::new().expect("tempdir");
        // Simulate a repo with a long hyphenated name
        let repo_dir = temp.path().join("acme-widgets");
        let beads_dir = repo_dir.join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&jsonl_path, "").expect("write empty jsonl");

        let bootstrap_layer = ConfigLayer::default();
        let prefix = resolve_bootstrap_issue_prefix(&bootstrap_layer, &beads_dir, &jsonl_path)
            .expect("prefix");
        assert_eq!(prefix, "aw");
    }

    #[test]
    fn resolve_bootstrap_issue_prefix_keeps_short_dir_name() {
        let temp = TempDir::new().expect("tempdir");
        let repo_dir = temp.path().join("myrepo");
        let beads_dir = repo_dir.join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&jsonl_path, "").expect("write empty jsonl");

        let bootstrap_layer = ConfigLayer::default();
        let prefix = resolve_bootstrap_issue_prefix(&bootstrap_layer, &beads_dir, &jsonl_path)
            .expect("prefix");
        assert_eq!(prefix, "myrepo");
    }

    #[test]
    fn open_storage_with_cli_recovers_corrupt_db_from_valid_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"not a sqlite database").expect("write corrupt db");
        write_single_issue_jsonl(&jsonl_path, "bd-recover1", "Recovered from JSONL");

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-recover1")
            .expect("query issue")
            .expect("issue should exist after recovery");

        assert_eq!(issue.title, "Recovered from JSONL");
        assert!(!storage_ctx.no_db);
        assert!(db_path.is_file(), "recovered database should exist");

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            backups.iter().any(|name| {
                name.starts_with("beads.db.")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
            }),
            "original database should be preserved in the recovery directory"
        );

        drop(storage_ctx);

        let reopened_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("reopen storage");
        let reopened_issue = reopened_ctx
            .storage
            .get_issue("bd-recover1")
            .expect("query reopened issue")
            .expect("issue should remain readable after reopening");
        assert_eq!(reopened_issue.title, "Recovered from JSONL");
    }

    #[test]
    fn open_storage_with_cli_recovers_malformed_schema_db_from_valid_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        create_malformed_blocked_cache_db(&db_path);
        write_single_issue_jsonl(
            &jsonl_path,
            "bd-rmalf1",
            "Recovered from malformed schema DB",
        );

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-rmalf1")
            .expect("query issue")
            .expect("issue should exist after malformed-schema recovery");

        assert_eq!(issue.title, "Recovered from malformed schema DB");
        assert!(!storage_ctx.no_db);
        assert!(db_path.is_file(), "recovered database should exist");

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            backups.iter().any(|name| {
                name.starts_with("beads.db.")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
            }),
            "malformed original database should be preserved in the recovery directory"
        );
    }

    #[test]
    fn open_storage_with_cli_recovers_malformed_schema_db_with_in_progress_issue() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        create_malformed_blocked_cache_db(&db_path);
        let issue = Issue {
            id: "beads_rust-3h0h".to_string(),
            title: "Auto-recover malformed blocked_issues_cache schema from JSONL".to_string(),
            status: Status::InProgress,
            priority: Priority::CRITICAL,
            issue_type: IssueType::Bug,
            created_at: chrono::DateTime::parse_from_rfc3339("2026-03-08T22:47:27.836536089Z")
                .expect("parse created_at")
                .with_timezone(&Utc),
            updated_at: chrono::DateTime::parse_from_rfc3339("2026-03-08T22:47:30.925913142Z")
                .expect("parse updated_at")
                .with_timezone(&Utc),
            created_by: Some("ubuntu".to_string()),
            source_repo: Some(".".to_string()),
            compaction_level: Some(0),
            original_size: Some(0),
            ..Issue::default()
        };
        write_issue_jsonl(&jsonl_path, &issue);

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let recovered_issue = storage_ctx
            .storage
            .get_issue("beads_rust-3h0h")
            .expect("query issue")
            .expect("issue should exist after malformed-schema recovery");

        assert_eq!(
            recovered_issue.title,
            "Auto-recover malformed blocked_issues_cache schema from JSONL"
        );
        assert_eq!(recovered_issue.status, Status::InProgress);
        assert_eq!(recovered_issue.priority, Priority::CRITICAL);
        assert_eq!(recovered_issue.issue_type, IssueType::Bug);
        assert_eq!(recovered_issue.created_by.as_deref(), Some("ubuntu"));
        assert_eq!(recovered_issue.source_repo.as_deref(), Some("."));
    }

    #[test]
    fn open_storage_with_cli_recovers_when_post_open_probe_finds_duplicate_config_rows() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let mut storage = SqliteStorage::open(&db_path).expect("create seed db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed issue prefix");
        drop(storage);
        insert_duplicate_issue_prefix_config_row(&db_path, "bd");

        write_single_issue_jsonl(
            &jsonl_path,
            "bd-rdup01",
            "Recovered from duplicate config rows",
        );

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-rdup01")
            .expect("query issue")
            .expect("issue should exist after duplicate-config recovery");

        assert_eq!(issue.title, "Recovered from duplicate config rows");
        assert!(!storage_ctx.no_db);

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            backups.iter().any(|name| {
                name.starts_with("beads.db.")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
            }),
            "duplicate-config database should be preserved in the recovery directory"
        );
    }

    #[test]
    fn open_storage_with_cli_recovers_using_resolved_external_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        fs::write(&db_path, b"not a sqlite database").expect("write corrupt db");
        write_single_issue_jsonl(&jsonl_path, "bd-rxtrn1", "Recovered from external JSONL");

        let cli = CliOverrides {
            db: Some(db_path),
            ..CliOverrides::default()
        };
        let storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-rxtrn1")
            .expect("query issue")
            .expect("issue should exist after recovery");

        assert_eq!(issue.title, "Recovered from external JSONL");
        assert_eq!(storage_ctx.paths.jsonl_path, jsonl_path);
    }

    #[test]
    fn open_storage_with_cli_rebuilds_missing_db_from_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        write_single_issue_jsonl(&jsonl_path, "bd-recovered", "Recovered from JSONL only");

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-recovered")
            .expect("query issue")
            .expect("issue should exist after rebuild");

        assert_eq!(issue.title, "Recovered from JSONL only");
        assert!(db_path.is_file(), "database should be rebuilt from JSONL");
    }

    #[test]
    fn open_storage_with_cli_no_db_supports_external_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        write_single_issue_jsonl(&jsonl_path, "bd-extimp", "Imported from external JSONL");

        let cli = CliOverrides {
            db: Some(db_path),
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");
        let imported = storage_ctx
            .storage
            .get_issue("bd-extimp")
            .expect("query imported issue")
            .expect("issue should be imported");
        assert_eq!(imported.title, "Imported from external JSONL");

        let new_issue = Issue {
            id: "bd-extflsh".to_string(),
            title: "Flushed to external JSONL".to_string(),
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&new_issue, "tester")
            .expect("create issue");
        storage_ctx
            .flush_no_db_if_dirty()
            .expect("flush no-db export");

        let exported = fs::read_to_string(&jsonl_path).expect("read external jsonl");
        assert!(
            exported.contains("\"id\":\"bd-extflsh\""),
            "flush should export to the resolved external JSONL path"
        );
    }

    #[test]
    fn open_storage_with_cli_no_db_keeps_distinct_closed_issues_with_identical_content() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let jsonl_path = beads_dir.join("issues.jsonl");

        let now = Utc::now();
        let first = Issue {
            id: "bd-a1111".to_string(),
            title: "Same title".to_string(),
            status: Status::Closed,
            created_at: now,
            updated_at: now,
            closed_at: Some(now),
            close_reason: Some("fixed".to_string()),
            ..Issue::default()
        };
        let second = Issue {
            id: "bd-b2222".to_string(),
            title: "Same title".to_string(),
            status: Status::Closed,
            created_at: now + chrono::Duration::minutes(1),
            updated_at: now + chrono::Duration::minutes(1),
            closed_at: Some(now + chrono::Duration::minutes(1)),
            close_reason: Some("duplicate".to_string()),
            ..Issue::default()
        };
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&first).expect("serialize first issue"),
            serde_json::to_string(&second).expect("serialize second issue")
        );
        fs::write(&jsonl_path, content).expect("write jsonl");

        let cli = CliOverrides {
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");

        let first_loaded = storage_ctx
            .storage
            .get_issue("bd-a1111")
            .expect("query first issue");
        let second_loaded = storage_ctx
            .storage
            .get_issue("bd-b2222")
            .expect("query second issue");
        assert!(
            first_loaded.is_some(),
            "first duplicate issue should remain addressable"
        );
        assert!(
            second_loaded.is_some(),
            "second duplicate issue should remain addressable"
        );
    }

    #[test]
    fn implicit_external_jsonl_allowed_requires_external_db_family() {
        let beads_dir = PathBuf::from("/tmp/project/.beads");
        let local_db = beads_dir.join("beads.db");
        let external_jsonl = PathBuf::from("/tmp/external/issues.jsonl");
        assert!(!implicit_external_jsonl_allowed(
            &beads_dir,
            &local_db,
            &external_jsonl
        ));

        let external_db = PathBuf::from("/tmp/external/beads.db");
        assert!(implicit_external_jsonl_allowed(
            &beads_dir,
            &external_db,
            &external_jsonl
        ));
    }

    #[test]
    fn database_snapshot_keeps_live_sidecars_absent() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let db_path = beads_dir.join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).expect("open db");
            storage
                .set_config("issue_prefix", "bd")
                .expect("write config");
        }

        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        let journal_path = PathBuf::from(format!("{}-journal", db_path.to_string_lossy()));
        let _ = fs::remove_file(&wal_path);
        let _ = fs::remove_file(&shm_path);
        let _ = fs::remove_file(&journal_path);

        let prefix = with_database_family_snapshot(&db_path, |snapshot_db_path| {
            let conn = fsqlite::Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
            let row = conn.query_row("SELECT value FROM config WHERE key = 'issue_prefix'")?;
            Ok(row
                .get(0)
                .and_then(fsqlite_types::SqliteValue::as_text)
                .map(str::to_string))
        })
        .expect("read snapshot");

        assert_eq!(prefix.as_deref(), Some("bd"));
        assert!(
            !wal_path.exists() && !shm_path.exists() && !journal_path.exists(),
            "snapshot reads must not create live sidecar files"
        );
    }

    #[test]
    fn open_storage_with_cli_no_db_flushes_force_flush_without_dirty_rows() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        write_single_issue_jsonl(&jsonl_path, "bd-purge", "Issue to purge");

        let cli = CliOverrides {
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");
        storage_ctx
            .storage
            .purge_issue("bd-purge", "tester")
            .expect("purge issue");

        assert_eq!(
            storage_ctx
                .storage
                .get_dirty_issue_count()
                .expect("dirty issue count"),
            0,
            "hard delete removes the dirty row, so the flush gate must also honor needs_flush"
        );
        assert_eq!(
            storage_ctx
                .storage
                .get_metadata("needs_flush")
                .expect("needs_flush metadata")
                .as_deref(),
            Some("true")
        );

        storage_ctx
            .flush_no_db_if_dirty()
            .expect("flush no-db hard delete");

        let exported = fs::read_to_string(&jsonl_path).expect("read exported jsonl");
        assert!(
            !exported.contains("\"id\":\"bd-purge\""),
            "force-flush deletes must update JSONL even when no dirty rows remain"
        );
    }

    #[test]
    fn open_storage_with_cli_no_db_refuses_to_flush_stale_snapshot() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        write_single_issue_jsonl(&jsonl_path, "bd-extimp", "Imported from external JSONL");

        let cli = CliOverrides {
            db: Some(db_path),
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");

        let new_issue = Issue {
            id: "bd-extflsh".to_string(),
            title: "Flushed to external JSONL".to_string(),
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&new_issue, "tester")
            .expect("create issue");

        write_single_issue_jsonl(&jsonl_path, "bd-concurrent", "Concurrent rewrite");

        let err = storage_ctx
            .flush_no_db_if_dirty()
            .expect_err("stale flush conflict");
        assert!(matches!(err, BeadsError::SyncConflict { .. }));

        let exported = fs::read_to_string(&jsonl_path).expect("read external jsonl");
        assert!(
            exported.contains("\"id\":\"bd-concurrent\""),
            "concurrent JSONL content should be preserved"
        );
        assert!(
            !exported.contains("\"id\":\"bd-extflsh\""),
            "stale in-memory edits should not overwrite concurrent JSONL changes"
        );
    }

    #[test]
    fn open_storage_with_cli_no_db_does_not_render_after_flush_conflict() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        write_single_issue_jsonl(&jsonl_path, "bd-extimp", "Imported from external JSONL");

        let cli = CliOverrides {
            db: Some(db_path),
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");

        let new_issue = Issue {
            id: "bd-extflsh".to_string(),
            title: "Flushed to external JSONL".to_string(),
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&new_issue, "tester")
            .expect("create issue");

        write_single_issue_jsonl(&jsonl_path, "bd-concurrent", "Concurrent rewrite");

        let rendered = std::cell::Cell::new(false);
        let err = storage_ctx
            .flush_no_db_then(|_| {
                rendered.set(true);
                Ok(())
            })
            .expect_err("stale flush conflict");

        assert!(matches!(err, BeadsError::SyncConflict { .. }));
        assert!(
            !rendered.get(),
            "render closure must not run after a failed no-db flush"
        );
    }

    #[test]
    fn open_storage_with_cli_no_db_does_not_update_last_touched_after_flush_conflict() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let db_path = external_dir.join("beads.db");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        write_single_issue_jsonl(&jsonl_path, "bd-extimp", "Imported from external JSONL");
        crate::util::set_last_touched_id(&beads_dir, "bd-existing");

        let cli = CliOverrides {
            db: Some(db_path),
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");

        let new_issue = Issue {
            id: "bd-extflsh".to_string(),
            title: "Flushed to external JSONL".to_string(),
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&new_issue, "tester")
            .expect("create issue");

        write_single_issue_jsonl(&jsonl_path, "bd-concurrent", "Concurrent rewrite");

        let err = storage_ctx
            .flush_no_db_then(|ctx| {
                crate::util::set_last_touched_id(&ctx.paths.beads_dir, "bd-extflsh");
                Ok(())
            })
            .expect_err("stale flush conflict");

        assert!(matches!(err, BeadsError::SyncConflict { .. }));
        assert_eq!(
            crate::util::get_last_touched_id(&beads_dir),
            "bd-existing",
            "failed no-db flush must not leave a stale last-touched pointer behind"
        );
    }

    #[test]
    fn open_storage_with_cli_backs_up_non_file_sidecars_that_block_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_dir = beads_dir.join("beads.db-wal");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&wal_dir).expect("create fake wal dir");

        fs::write(&db_path, b"not a sqlite database").expect("write corrupt db");
        write_single_issue_jsonl(&jsonl_path, "bd-recover2", "Recovered with odd sidecar");
        fs::write(wal_dir.join("sentinel.txt"), "keep me").expect("write sentinel");

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-recover2")
            .expect("query issue")
            .expect("issue should exist after recovery");

        assert_eq!(issue.title, "Recovered with odd sidecar");
        assert!(
            !wal_dir.join("sentinel.txt").exists(),
            "the original blocking wal directory should be moved away rather than reused in place"
        );

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let wal_backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("beads.db-wal.")
                            && Path::new(name)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
                    })
            })
            .collect();
        assert_eq!(
            wal_backups.len(),
            1,
            "wal directory should be backed up once"
        );
        assert_eq!(
            fs::read_to_string(wal_backups[0].join("sentinel.txt"))
                .expect("read backed-up sentinel"),
            "keep me"
        );
    }

    #[test]
    fn open_storage_result_load_config_matches_direct_load_config() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(
            beads_dir.join("config.yaml"),
            "issue_prefix: proj\ncolor: false\n",
        )
        .expect("write project config");

        let cli = CliOverrides {
            display_color: Some(false),
            ..CliOverrides::default()
        };
        let mut storage_ctx = open_storage_with_cli(&beads_dir, &cli).expect("storage");
        storage_ctx
            .storage
            .set_config("issue_prefix", "db-prefix")
            .expect("set issue_prefix");

        let direct =
            load_config(&beads_dir, Some(&storage_ctx.storage), &cli).expect("direct load config");
        let reused = storage_ctx
            .load_config(&cli)
            .expect("reused startup load config");

        assert_eq!(reused, direct);
    }

    #[test]
    fn open_storage_with_cli_backs_up_rollback_journal_sidecars_during_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let journal_dir = beads_dir.join("beads.db-journal");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&journal_dir).expect("create fake journal dir");

        fs::write(&db_path, b"not a sqlite database").expect("write corrupt db");
        write_single_issue_jsonl(&jsonl_path, "bd-rjrnl1", "Recovered with journal");
        fs::write(journal_dir.join("sentinel.txt"), "keep me").expect("write sentinel");

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-rjrnl1")
            .expect("query issue")
            .expect("issue should exist after recovery");

        assert_eq!(issue.title, "Recovered with journal");
        assert!(
            !journal_dir.join("sentinel.txt").exists(),
            "the original rollback journal sidecar should be moved out of the way during recovery"
        );

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let journal_backups: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("beads.db-journal.")
                            && Path::new(name)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
                    })
            })
            .collect();
        assert_eq!(
            journal_backups.len(),
            1,
            "rollback journal should be backed up once"
        );
        assert_eq!(
            fs::read_to_string(journal_backups[0].join("sentinel.txt"))
                .expect("read backed-up sentinel"),
            "keep me"
        );
    }

    #[test]
    fn open_storage_with_cli_does_not_recover_from_invalid_jsonl() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"not a sqlite database").expect("write corrupt db");
        fs::write(&jsonl_path, "{not valid json\n").expect("write invalid jsonl");

        let err =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect_err("should fail");
        assert!(
            matches!(err, BeadsError::Database(_)),
            "invalid JSONL should preserve the original database open error"
        );
        assert!(
            db_path.is_file(),
            "original database should remain in place"
        );

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        let backup_count =
            fs::read_dir(&recovery_dir).map_or(0, |entries| entries.flatten().count());
        assert_eq!(
            backup_count, 0,
            "no recovery backup should be created when JSONL preflight fails"
        );
    }

    #[test]
    fn move_database_family_to_recovery_rolls_back_partial_failure() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db").expect("write db");
        fs::write(&wal_path, b"wal").expect("write wal");

        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir).expect("create recovery dir");

        let stamp = "fixed-stamp";
        let conflicting_wal_backup =
            recovery_dir.join(recovery_backup_filename(&wal_path, stamp, "bak"));
        fs::create_dir_all(&conflicting_wal_backup).expect("create conflicting wal backup dir");

        let err =
            move_database_family_to_recovery(&db_path, &beads_dir, stamp).expect_err("should fail");
        assert!(matches!(err, BeadsError::Io(_)));

        assert!(db_path.is_file(), "db should be restored after rollback");
        assert!(wal_path.is_file(), "wal should remain after rollback");

        let db_backup = recovery_dir.join(recovery_backup_filename(&db_path, stamp, "bak"));
        assert!(
            !db_backup.exists(),
            "rolled back db backup should not remain in recovery dir"
        );
        assert!(
            conflicting_wal_backup.is_dir(),
            "the pre-existing conflicting path should be untouched"
        );
    }

    #[test]
    fn restore_database_family_after_failed_rebuild_rolls_back_partial_rebuild_staging() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"original-db").expect("write original db");
        fs::write(&wal_path, b"original-wal").expect("write original wal");
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir).expect("create recovery dir");

        // Use a specific backup file path that does NOT exist, so the
        // restore step fails with a missing-backup error.
        let wal_backup_file =
            recovery_dir.join(recovery_backup_filename(&wal_path, "fixed-stamp", "bak"));

        let err = restore_database_family_after_failed_rebuild(&RecoveryBackupSet {
            db_path: db_path.clone(),
            recovery_dir: recovery_dir.clone(),
            stamp: "fixed-stamp".to_string(),
            files: vec![(wal_path.clone(), wal_backup_file.clone())],
        })
        .expect_err("missing backup should fail restore");
        assert!(
            matches!(err, BeadsError::WithContext { .. }),
            "missing backup should surface a contextual recovery error"
        );
        assert!(
            should_surface_recovery_error(&err),
            "missing backup during restore should not be hidden behind the original open error"
        );
        assert!(
            err.to_string().contains("expected")
                && err
                    .to_string()
                    .contains(&wal_backup_file.display().to_string()),
            "unexpected error: {err}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read rebuilt db"),
            b"original-db",
            "rebuilt db should remain in place after rollback"
        );
        assert_eq!(
            fs::read(&wal_path).expect("read rebuilt wal"),
            b"original-wal",
            "rebuilt wal should remain in place after rollback"
        );
        let rebuild_failed_wal = recovery_dir.join(recovery_backup_filename(
            &wal_path,
            "fixed-stamp",
            "rebuild-failed",
        ));
        assert!(
            !rebuild_failed_wal.exists(),
            "rolled back wal backup should not remain in recovery dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn move_database_family_to_recovery_backs_up_dangling_symlink_sidecars() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db").expect("write db");
        symlink("missing-wal-target", &wal_path).expect("create dangling wal symlink");

        let stamp = "fixed-stamp";
        let backup_set =
            move_database_family_to_recovery(&db_path, &beads_dir, stamp).expect("backup set");

        assert!(
            fs::symlink_metadata(&wal_path).is_err(),
            "dangling wal symlink should be moved out of the live database family"
        );
        let wal_backup = backup_set
            .files
            .iter()
            .find(|(original, _)| original == &wal_path)
            .map(|(_, backup)| backup)
            .expect("wal sidecar should be included in backup set");
        assert!(
            fs::symlink_metadata(wal_backup)
                .expect("wal backup metadata")
                .file_type()
                .is_symlink(),
            "dangling wal sidecar should remain a symlink in recovery"
        );
        assert_eq!(
            fs::read_link(wal_backup).expect("read wal backup symlink"),
            PathBuf::from("missing-wal-target")
        );
    }
}
