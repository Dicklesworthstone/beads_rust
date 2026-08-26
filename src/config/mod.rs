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
use crate::sync::path::validate_sync_path_with_external;
use crate::sync::{
    ExpectedJsonlSourceRef, ExportConfig, FreshDatabaseReplacementWitness, ImportConfig,
    ImportResult, JsonlSourceSnapshot, JsonlSourceStateWitness, JsonlTombstoneFilter,
    PreservedIssue, auto_flush, blocking_database_family_write_lock_with_timeout,
    capture_optional_jsonl_source, dirty_issues_missing_from_jsonl,
    export_to_jsonl_with_policy_expected_under_authority, finalize_export_under_authority,
    import_from_jsonl_snapshot, import_from_jsonl_snapshot_into_fresh_replacement,
    preflight_import_snapshot, restore_dirty_issues_after_rebuild,
    restore_tombstones_after_rebuild, scan_jsonl_snapshot_for_tombstone_filter,
    snapshot_dirty_live_issues, snapshot_tombstones, tombstones_missing_from_jsonl_tombstones,
    verify_jsonl_source_snapshot_current,
};
use crate::util::id::{
    IdConfig, abbreviate_prefix, normalize_configured_prefix, normalize_prefix, parse_id,
    split_prefix_remainder,
};
use chrono::Utc;
use fsqlite_error::FrankenError;
use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::hex_encode;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
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
const SYMLINKED_DB_RECOVERY_ERROR_PREFIX: &str =
    "refusing to rebuild symlinked SQLite database path";

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
    #[serde(default = "default_database_filename")]
    pub database: String,
    #[serde(default = "default_jsonl_export_filename")]
    pub jsonl_export: String,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub deletions_retention_days: Option<u64>,
}

fn default_database_filename() -> String {
    DEFAULT_DB_FILENAME.to_string()
}

fn default_jsonl_export_filename() -> String {
    DEFAULT_JSONL_FILENAME.to_string()
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            database: default_database_filename(),
            jsonl_export: default_jsonl_export_filename(),
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        // Resolve an explicit database authority once before deriving either
        // member of the DB/JSONL family. Keeping a raw `subdir/../.beads`
        // spelling here made the database and its sibling JSONL disagree
        // with the already-canonical workspace route, and the JSONL safety
        // boundary then correctly refused the surviving traversal token.
        let normalized_db_override = db_override.map(|path| normalize_db_override_path(path));
        let normalized_db_override_ref = normalized_db_override.as_ref();
        let db_path = resolve_db_path(beads_dir, &metadata, normalized_db_override_ref);
        let jsonl_path = resolve_jsonl_path(beads_dir, &metadata, normalized_db_override_ref);

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

fn normalize_db_override_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return canonical;
    }

    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };

    dunce::canonicalize(parent)
        .map(|canonical_parent| canonical_parent.join(file_name))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Discover the active `.beads` directory.
///
/// Honors `BEADS_DIR` when set, otherwise walks up from `start` (or CWD).
///
/// # Errors
///
/// Returns an error if no beads directory is found or the CWD cannot be read.
pub fn discover_beads_dir(start: Option<&Path>) -> Result<PathBuf> {
    let env_override = beads_dir_override_from_env();
    discover_beads_dir_with_env(start, env_override.as_deref())
}

fn discover_beads_dir_with_env(
    start: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    discover_beads_dir_with_env_and_ceiling(start, env_override, None)
}

fn discover_beads_dir_with_env_and_ceiling(
    start: Option<&Path>,
    env_override: Option<&Path>,
    discovery_ceiling: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_override {
        return resolve_explicit_beads_dir(path, "BEADS_DIR");
    }

    let candidate =
        discover_beads_dir_candidate_with_env_and_ceiling(start, None, discovery_ceiling)?;
    routing::follow_redirects(&candidate, 10)
}

fn discover_beads_dir_candidate_with_env(
    start: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    discover_beads_dir_candidate_with_env_and_ceiling(start, env_override, None)
}

fn discover_beads_dir_candidate_with_env_and_ceiling(
    start: Option<&Path>,
    env_override: Option<&Path>,
    discovery_ceiling: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_override {
        return validate_explicit_beads_dir(path, "BEADS_DIR");
    }

    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => env::current_dir()?,
    };

    // First `.git` FILE seen on the walk: the marker of a linked git
    // worktree (a primary checkout has a `.git` directory). Used for the
    // common-dir fallback below when the walk finds no workspace at all.
    let mut linked_worktree_git_file: Option<PathBuf> = None;

    loop {
        let candidate = current.join(".beads");
        if candidate.is_dir() {
            // Linked-worktree resolution (GitHub #429's underlying bug): a
            // fresh linked worktree checks out the TRACKED half of `.beads`
            // (issues.jsonl, metadata.json, config.yaml) but not the local
            // database. Treating that snapshot as a workspace would rebuild
            // a private database from potentially stale JSONL and split
            // state from the primary checkout. When the candidate has no
            // local database and no explicit redirect, resolve through the
            // worktree's `.git` file to the primary checkout's `.beads`.
            if let Some(primary) = linked_worktree_primary_beads_dir(&current, &candidate) {
                return Ok(primary);
            }
            return Ok(candidate);
        }
        let candidate_underscore = current.join("_beads");
        if candidate_underscore.is_dir() {
            return Ok(candidate_underscore);
        }

        if linked_worktree_git_file.is_none() {
            let git_path = current.join(".git");
            if git_path.is_file() {
                linked_worktree_git_file = Some(git_path);
            }
        }

        if discovery_ceiling.is_some_and(|ceiling| current == ceiling) {
            break;
        }
        if !current.pop() {
            break;
        }
    }

    // No workspace anywhere up the walk, but we passed a linked-worktree
    // marker: worktrees checked out OUTSIDE the primary tree (e.g.
    // `git worktree add ../feature-x`) cannot reach the primary `.beads`
    // by walking up, so resolve it through the worktree's gitdir instead.
    if let Some(git_file) = linked_worktree_git_file
        && let Some(primary) = primary_beads_dir_from_git_file(&git_file)
    {
        return Ok(primary);
    }

    Err(BeadsError::NotInitialized)
}

/// Decide whether a discovered `.beads` candidate inside a linked git
/// worktree should resolve to the primary checkout's `.beads` instead.
///
/// Returns `Some(primary)` only when ALL of the following hold, keeping the
/// resolution conservative:
///
/// - `workspace_root/.git` is a FILE (linked-worktree marker; the primary
///   checkout has a `.git` directory and is unaffected);
/// - the candidate has no explicit `redirect` file (an explicit redirect is
///   the operator's decision and is honored by `routing::follow_redirects`);
/// - the candidate has no local database (a worktree where `br init` ran, or
///   that already imported once, keeps its own store — no rug-pull);
/// - the primary checkout's `.beads` actually exists and is a different
///   directory.
///
/// Never spawns git: the primary checkout is derived by parsing the
/// worktree's `.git` file and gitdir `commondir` pointer.
fn linked_worktree_primary_beads_dir(workspace_root: &Path, candidate: &Path) -> Option<PathBuf> {
    let git_file = workspace_root.join(".git");
    if !git_file.is_file() {
        return None;
    }
    if candidate.join("redirect").is_file() {
        return None;
    }
    if workspace_has_local_database(candidate) {
        return None;
    }
    let primary = primary_beads_dir_from_git_file(&git_file)?;
    let same = match (
        dunce::canonicalize(&primary),
        dunce::canonicalize(candidate),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => primary == *candidate,
    };
    if same { None } else { Some(primary) }
}

/// True when the workspace has its own local database file — i.e. it is a
/// real initialized workspace rather than a tracked-artifact-only snapshot.
///
/// The database filename comes from `metadata.json`'s `database` key,
/// defaulting to `beads.db` when metadata is absent or unreadable.
fn workspace_has_local_database(beads_dir: &Path) -> bool {
    let db_name = fs::read_to_string(beads_dir.join("metadata.json"))
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|value| {
            value
                .get("database")
                .and_then(|db| db.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "beads.db".to_string());
    beads_dir.join(db_name).is_file()
}

/// Resolve the primary checkout's `.beads` directory from a linked git
/// worktree's `.git` file, without spawning git.
///
/// The `.git` file contains `gitdir: <path-to>/.git/worktrees/<name>`; the
/// gitdir's `commondir` file (written by git for every linked worktree)
/// points back at the shared `.git` directory, whose parent is the primary
/// working tree. Falls back to stripping a trailing `worktrees/<name>` when
/// `commondir` is missing. Bare repositories (common dir not named `.git`)
/// resolve to `None` — there is no primary working tree to share.
fn primary_beads_dir_from_git_file(git_file: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_file).ok()?;
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    if gitdir.is_empty() {
        return None;
    }
    let gitdir_path = {
        let raw = Path::new(gitdir);
        if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            git_file.parent()?.join(raw)
        }
    };

    let common_git_dir = fs::read_to_string(gitdir_path.join("commondir"))
        .ok()
        .and_then(|pointer| {
            let trimmed = pointer.trim();
            if trimmed.is_empty() {
                return None;
            }
            let raw = Path::new(trimmed);
            if raw.is_absolute() {
                Some(raw.to_path_buf())
            } else {
                Some(gitdir_path.join(raw))
            }
        })
        .or_else(|| {
            let worktrees = gitdir_path.parent()?;
            if worktrees.file_name()? == "worktrees" {
                worktrees.parent().map(Path::to_path_buf)
            } else {
                None
            }
        })?;
    let common_git_dir = dunce::canonicalize(&common_git_dir).unwrap_or(common_git_dir);
    if common_git_dir.file_name()? != ".git" {
        return None;
    }
    let primary = common_git_dir.parent()?.join(".beads");
    if primary.is_dir() {
        Some(primary)
    } else {
        None
    }
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
    let beads_dir_env_override = beads_dir_override_from_env();
    let db_env_override = startup_db_override_from_env();
    discover_beads_dir_with_cli_from(
        None,
        cli,
        beads_dir_env_override.as_deref(),
        db_env_override.as_deref(),
    )
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
    let beads_dir_env_override = beads_dir_override_from_env();
    let db_env_override = startup_db_override_from_env();
    match discover_beads_dir_with_cli_from(
        None,
        cli,
        beads_dir_env_override.as_deref(),
        db_env_override.as_deref(),
    ) {
        Ok(path) => Ok(Some(path)),
        Err(BeadsError::NotInitialized) if cli.db.is_none() => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn discover_optional_beads_dir_candidate_with_cli(
    cli: &CliOverrides,
) -> Result<Option<PathBuf>> {
    let beads_dir_env_override = beads_dir_override_from_env();
    let db_env_override = startup_db_override_from_env();
    match discover_beads_dir_candidate_with_cli_from(
        None,
        cli,
        beads_dir_env_override.as_deref(),
        db_env_override.as_deref(),
    ) {
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
    discover_beads_dir_with_cli_from_and_ceiling(
        start,
        cli,
        beads_dir_env_override,
        db_env_override,
        None,
    )
}

fn discover_beads_dir_with_cli_from_and_ceiling(
    start: Option<&Path>,
    cli: &CliOverrides,
    beads_dir_env_override: Option<&Path>,
    db_env_override: Option<&Path>,
    discovery_ceiling: Option<&Path>,
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

    let startup_db_override = db_env_override.map(Path::to_path_buf);

    if let Some(db_path) = startup_db_override.as_deref()
        && let Ok(beads_dir) = derive_beads_dir_from_db_path(db_path)
    {
        return resolve_explicit_beads_dir(
            &beads_dir,
            &format!("database override '{}'", db_path.display()),
        );
    }

    discover_beads_dir_with_env_and_ceiling(
        start,
        beads_dir_env_override,
        discovery_ceiling,
    )
    .map_err(|err| {
        match (
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
        }
    })
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

    let startup_db_override = db_env_override.map(Path::to_path_buf);

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

fn beads_dir_override_from_env() -> Option<PathBuf> {
    env::var("BEADS_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
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

#[derive(Debug, Clone)]
struct RecoveryBackupSet {
    db_path: PathBuf,
    recovery_dir: PathBuf,
    stamp: String,
    had_original_database: bool,
    files: Vec<RecoveryBackupPath>,
    verified_files: Vec<RecoveryBackupVerification>,
}

type RecoveryBackupPath = (PathBuf, PathBuf);
type VerifiedRecoveryBackupBatch = (Vec<RecoveryBackupPath>, Vec<RecoveryBackupVerification>);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct RecoveryBackupVerification {
    pub original: String,
    pub backup: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryArtifactFingerprint {
    kind: String,
    size_bytes: Option<u64>,
    sha256: Option<String>,
    symlink_target: Option<String>,
}

#[derive(Debug)]
struct RecoveryLivePathWitness {
    original: PathBuf,
    fingerprint: Option<RecoveryArtifactFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonlRecoveryStrategy {
    RebuildFromJsonl,
    DeferToExplicitImport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuccessfulRecoveryDisposition {
    FinalizeImmediately,
    RetainBackupUntilCommandSuccess,
}

#[derive(Debug)]
struct RecoveredJsonlSource {
    source: Arc<JsonlSourceSnapshot>,
    authority: Arc<crate::sync::JsonlFamilyWriteLock>,
}

#[derive(Debug)]
struct SqliteRecoveryOpenResult {
    storage: SqliteStorage,
    auto_rebuilt: bool,
    pending_recovery_backup: Option<RecoveryBackupSet>,
    recovered_jsonl: Option<RecoveredJsonlSource>,
}

#[derive(Debug, Clone, Copy)]
struct SqliteStartupOpenOptions<'a> {
    defer_jsonl_recovery: bool,
    read_only_fast_open: bool,
    write_authority: Option<&'a Arc<crate::sync::DatabaseFamilyWriteLock>>,
    allow_external_jsonl: bool,
}

fn open_sqlite_storage_with_recovery(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<SqliteRecoveryOpenResult> {
    open_sqlite_storage_with_recovery_strategy(
        beads_dir,
        paths,
        lock_timeout,
        bootstrap_layer,
        allow_external_jsonl,
        JsonlRecoveryStrategy::RebuildFromJsonl,
        write_authority,
    )
}

fn open_sqlite_storage_with_recovery_after_fast_open_miss(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
    allow_external_jsonl: bool,
) -> Result<(
    SqliteRecoveryOpenResult,
    Option<Arc<crate::sync::DatabaseFamilyWriteLock>>,
)> {
    let owned_write_authority = if write_authority.is_some() {
        None
    } else {
        let authority = Arc::new(blocking_database_family_write_lock_with_timeout(
            beads_dir,
            &paths.db_path,
            lock_timeout,
        )?);
        Some(authority)
    };
    let effective_authority = write_authority
        .or(owned_write_authority.as_ref())
        .ok_or_else(|| BeadsError::SyncConflict {
            message: "Writable fast-open fallback has no database authority".to_string(),
        })?;
    let database_was_missing = effective_authority.bind_database_inode_for_mutation()?;
    // Read-only fast-open commands legitimately skip the startup
    // pending-merge gate, so this writable fallback must re-impose the same
    // barriers under the authority it just acquired, before any open that
    // could recover or migrate: a valid database on a stale schema routes to
    // the reviewed migration workflow instead of auto-migrating, and a
    // pending sync merge refuses writable recovery until `br sync --merge`
    // reconciles it. Missing files and non-SQLite bytes classify Absent and
    // keep the normal recovery path.
    if !database_was_missing {
        match SqliteStorage::inspect_pending_sync_merge_under_authority(
            &paths.db_path,
            effective_authority,
        ) {
            Ok(crate::storage::sqlite::PendingSyncMergeInspection::Absent) => {}
            Ok(pending) => {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Refusing writable fast-open fallback because {}; run `br sync --merge` to resume and verify artifact reconciliation first",
                        pending.diagnostic()
                    ),
                });
            }
            Err(error @ BeadsError::SchemaMismatch { .. }) => {
                return Err(error.reviewed_schema_migration_required());
            }
            Err(error) => return Err(error),
        }
    }
    let open_result = if database_was_missing && paths.jsonl_path.is_file() {
        open_when_db_file_is_missing(
            beads_dir,
            paths,
            lock_timeout,
            bootstrap_layer,
            allow_external_jsonl,
            JsonlRecoveryStrategy::RebuildFromJsonl,
            Some(effective_authority),
        )?
    } else {
        open_sqlite_storage_with_recovery(
            beads_dir,
            paths,
            lock_timeout,
            bootstrap_layer,
            allow_external_jsonl,
            Some(effective_authority),
        )?
    };
    Ok((open_result, owned_write_authority))
}

fn open_sqlite_storage_with_deferred_jsonl_recovery(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<SqliteRecoveryOpenResult> {
    open_sqlite_storage_with_recovery_strategy(
        beads_dir,
        paths,
        lock_timeout,
        bootstrap_layer,
        allow_external_jsonl,
        JsonlRecoveryStrategy::DeferToExplicitImport,
        write_authority,
    )
}

fn open_sqlite_storage_with_recovery_strategy(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    recovery_strategy: JsonlRecoveryStrategy,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<SqliteRecoveryOpenResult> {
    if !paths.db_path.is_file() && paths.jsonl_path.is_file() {
        return open_when_db_file_is_missing(
            beads_dir,
            paths,
            lock_timeout,
            bootstrap_layer,
            allow_external_jsonl,
            recovery_strategy,
            write_authority,
        );
    }
    let authority = write_authority.ok_or_else(|| BeadsError::SyncConflict {
        message: "Writable database recovery has no database-family authority".to_string(),
    })?;
    if !paths.db_path.is_file() {
        let _fresh_witness = authority.install_empty_database_replacement_and_bind()?;
        authority.verify_database_authority()?;
    }

    authority.verify_database_authority()?;
    quarantine_truncated_wal_sidecar(&paths.db_path, beads_dir);
    authority.verify_database_authority()?;

    let prepare_fresh_storage = || -> Result<(SqliteStorage, RecoveryBackupSet)> {
        prepare_fresh_storage_for_deferred_import(
            &paths.db_path,
            beads_dir,
            lock_timeout,
            authority,
        )
    };

    match SqliteStorage::open_with_timeout_under_write_authority(
        &paths.db_path,
        lock_timeout,
        authority,
    ) {
        Ok(storage) => match storage.detect_recoverable_open_anomaly() {
            Ok(None) => Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: false,
                pending_recovery_backup: None,
                recovered_jsonl: None,
            }),
            Ok(Some(anomaly)) => rebuild_or_defer_after_recoverable_anomaly(
                storage,
                &anomaly,
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                recovery_strategy,
                write_authority,
                &prepare_fresh_storage,
            ),
            Err(probe_err) => {
                if !should_attempt_jsonl_recovery_after_open(
                    &probe_err,
                    &paths.db_path,
                    &paths.jsonl_path,
                ) {
                    return Err(probe_err);
                }
                rebuild_or_defer_after_probe_error(
                    storage,
                    &probe_err,
                    beads_dir,
                    paths,
                    lock_timeout,
                    bootstrap_layer,
                    allow_external_jsonl,
                    recovery_strategy,
                    write_authority,
                    &prepare_fresh_storage,
                )
            }
        },
        Err(open_err) => {
            if !should_attempt_jsonl_recovery(&open_err, &paths.db_path, &paths.jsonl_path) {
                return Err(open_err);
            }
            rebuild_or_defer_after_open_error(
                open_err,
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                recovery_strategy,
                write_authority,
                &prepare_fresh_storage,
            )
        }
    }
}

/// Handle the "DB file missing, JSONL present" case: either rebuild the
/// DB from JSONL outright, or (for the deferred-recovery path) prepare a
/// cleanup set and let the caller's explicit import populate a fresh DB.
fn open_when_db_file_is_missing(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    recovery_strategy: JsonlRecoveryStrategy,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<SqliteRecoveryOpenResult> {
    match recovery_strategy {
        JsonlRecoveryStrategy::RebuildFromJsonl => {
            let (storage, recovered_jsonl) = rebuild_database_from_jsonl(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                write_authority,
            )?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: true,
                pending_recovery_backup: None,
                recovered_jsonl: Some(recovered_jsonl),
            })
        }
        JsonlRecoveryStrategy::DeferToExplicitImport => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                "Database file is missing; deferring JSONL recovery to explicit import semantics"
            );
            let authority = write_authority.ok_or_else(|| BeadsError::SyncConflict {
                message: "Missing-database deferred recovery has no database-family authority"
                    .to_string(),
            })?;
            let (storage, cleanup_set) = prepare_fresh_storage_for_deferred_import(
                &paths.db_path,
                beads_dir,
                lock_timeout,
                authority,
            )?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: false,
                pending_recovery_backup: Some(cleanup_set),
                recovered_jsonl: None,
            })
        }
    }
}

/// Issue #228: proactively quarantine truncated WAL sidecar files before
/// opening. A WAL file that exists but is shorter than 32 bytes (the WAL
/// header size) cannot be valid and will cause frankensqlite to return
/// `WalCorrupt` during rebuild. Moving the sidecars out of the live
/// database family lets SQLite recreate them on the next write while
/// preserving the original bytes for operator inspection.
pub(crate) fn quarantine_truncated_wal_sidecar(db_path: &Path, beads_dir: &Path) {
    match fs::symlink_metadata(db_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            tracing::warn!(
                db_path = %db_path.display(),
                "Skipping truncated WAL quarantine for symlinked database path"
            );
            return;
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                db_path = %db_path.display(),
                error = %err,
                "Skipping truncated WAL quarantine because the database path could not be inspected"
            );
            return;
        }
    }

    let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
    let Ok(meta) = fs::metadata(&wal_path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    // A 0-byte WAL is the documented post-`PRAGMA wal_checkpoint(TRUNCATE)`
    // state — SqliteStorage::Drop runs that pragma on every mutating
    // invocation, so quarantining the empty file would re-pathologize the
    // healthy hand-off between two well-behaved processes (#291).
    if meta.len() == 0 {
        return;
    }
    if meta.len() >= 32 {
        return;
    }
    let wal_size = meta.len();
    let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
    match quarantine_database_artifacts(
        db_path,
        beads_dir,
        [wal_path.clone(), shm_path],
        "truncated-wal",
    ) {
        Ok(quarantined_paths) => {
            tracing::warn!(
                wal_path = %wal_path.display(),
                wal_size,
                quarantined_paths = ?quarantined_paths,
                "quarantined truncated WAL sidecar (< 32 bytes) before open"
            );
        }
        Err(err) => {
            tracing::warn!(
                wal_path = %wal_path.display(),
                wal_size,
                error = %err,
                "failed to quarantine truncated WAL sidecar before open"
            );
        }
    }
}

/// Back up the current database family and reopen a fresh handle. Used
/// by the deferred-import recovery path when we want to install a blank
/// DB and let an explicit `br sync --import-only` populate it.
fn prepare_fresh_storage_for_deferred_import(
    db_path: &Path,
    beads_dir: &Path,
    lock_timeout: Option<u64>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<(SqliteStorage, RecoveryBackupSet)> {
    let (mut storage, backup_set) = rebuild_database_family_with_backup(
        db_path,
        beads_dir,
        write_authority,
        SuccessfulRecoveryDisposition::RetainBackupUntilCommandSuccess,
        |fresh_witness| {
            write_authority.verify_fresh_database_replacement_witness(&fresh_witness)?;
            SqliteStorage::open_with_timeout_under_write_authority(
                db_path,
                lock_timeout,
                write_authority,
            )
        },
    )?;
    let recovery_dir = backup_set.recovery_dir.clone();
    write_authority.verify_database_authority()?;
    storage.attach_write_authority(Arc::clone(write_authority));
    warn!(
        db_path = %db_path.display(),
        recovery_dir = %recovery_dir.display(),
        "Prepared a fresh SQLite database; explicit import will populate it"
    );
    Ok((storage, backup_set))
}

/// Handle the `Err(open_err)` branch of the top-level open: either
/// rebuild the DB from JSONL or, on the deferred-import strategy, move
/// the broken DB family aside and open a fresh placeholder. The rebuild
/// arm prefers the original open error over a recovery error unless the
/// recovery error carries extra context worth surfacing.
#[allow(clippy::too_many_arguments)]
fn rebuild_or_defer_after_open_error(
    open_err: BeadsError,
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    recovery_strategy: JsonlRecoveryStrategy,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
    prepare_fresh_storage: &dyn Fn() -> Result<(SqliteStorage, RecoveryBackupSet)>,
) -> Result<SqliteRecoveryOpenResult> {
    match recovery_strategy {
        JsonlRecoveryStrategy::RebuildFromJsonl => {
            match rebuild_database_from_jsonl(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                write_authority,
            ) {
                Ok((storage, recovered_jsonl)) => Ok(SqliteRecoveryOpenResult {
                    storage,
                    auto_rebuilt: true,
                    pending_recovery_backup: None,
                    recovered_jsonl: Some(recovered_jsonl),
                }),
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
        JsonlRecoveryStrategy::DeferToExplicitImport => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                open_error = %open_err,
                "Open failed with a recoverable database error; deferring JSONL recovery to explicit import semantics"
            );
            let (storage, backup_set) = prepare_fresh_storage()?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: false,
                pending_recovery_backup: Some(backup_set),
                recovered_jsonl: None,
            })
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
    ) || matches!(
        open_err,
        // A member of the database family is a directory where the engine
        // expects a regular file (e.g. a `-journal` directory left by a
        // hostile or interrupted tool). fsqlite reports this as a plain I/O
        // error rather than a corruption variant, but it is exactly the
        // structural damage a JSONL rebuild repairs: recovery backs the whole
        // family up and recreates it.
        BeadsError::Database(FrankenError::Io(io_err))
            if io_err.kind() == std::io::ErrorKind::IsADirectory
    ) || matches!(
        open_err,
        // Exact runtime attestation can now reject a malformed current-version
        // table after the ordinary schema repair ran.  With an authoritative
        // JSONL source present, this is the same recoverable structural damage
        // as the older engine-level malformed-schema errors above.
        BeadsError::Config(detail)
            if detail == "runtime schema remains incompatible after repair"
    ) || matches!(
        open_err,
        // Schema preflight reports non-regular database-family members as a
        // fail-closed SyncConflict before fsqlite can turn them into an I/O
        // error.  Recovery moves the blocking member into the verified backup
        // set, so preserve the established directory-sidecar recovery path.
        BeadsError::SyncConflict { message }
            if message.starts_with("Refusing schema preflight because the ")
                && message.ends_with(" path is not a regular file")
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

/// Stable diagnostic prefix fsqlite emits when the `-wal-cert` sidecar holds
/// a durable-certificate record whose envelope version this engine build does
/// not decode (GitHub #441: upgrading `br` across an fsqlite engine
/// generation leaves the old sidecar behind, and every cert-regenerating
/// write fails with this message while reads stay healthy).
///
/// fsqlite reports the condition as `FrankenError::WalCorrupt` with the
/// version baked into a free-form `detail` string — there is no dedicated
/// typed variant to match on (verified against fsqlite-core 0.3.9
/// `wal_adapter.rs::validate_incomplete_certificate_suffix`), so this matches
/// the typed `WalCorrupt` variant plus this conservative substring. The
/// trailing version number is intentionally excluded so every unsupported
/// version (past or future) classifies the same way.
const FSQLITE_WAL_CERT_UNSUPPORTED_RECORD_VERSION_DETAIL: &str =
    "parallel WAL certificate suffix has unsupported record version";

/// True when a write failed because the parallel-WAL durability-certificate
/// sidecar (`-wal-cert`) was written by a different fsqlite engine
/// generation. The certificate is derived state: quarantining the sidecar
/// lets the engine regenerate it on the next write, whereas a full JSONL
/// rebuild is both heavier and unnecessary (and wedges entirely when the
/// JSONL merge anchor is stale, as in GitHub #441).
#[must_use]
pub(crate) fn is_stale_wal_certificate_version_error(err: &BeadsError) -> bool {
    matches!(
        err,
        BeadsError::Database(FrankenError::WalCorrupt { detail })
            if detail.contains(FSQLITE_WAL_CERT_UNSUPPORTED_RECORD_VERSION_DETAIL)
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

/// Handle the `Ok(Some(anomaly))` branch from
/// `detect_recoverable_open_anomaly`: either rebuild from JSONL while
/// preserving local unflushed tombstones, or move the DB family aside
/// and open a fresh placeholder for the explicit-import path.
///
/// Split out of `open_sqlite_storage_with_recovery_strategy` so the
/// caller stays under the pedantic `too_many_lines` budget; the
/// preservation comment lives here so the behavior is documented next to
/// the code that performs it.
#[allow(clippy::too_many_arguments)]
fn rebuild_or_defer_after_recoverable_anomaly(
    storage: SqliteStorage,
    anomaly: &str,
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    recovery_strategy: JsonlRecoveryStrategy,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
    prepare_fresh_storage: &dyn Fn() -> Result<(SqliteStorage, RecoveryBackupSet)>,
) -> Result<SqliteRecoveryOpenResult> {
    match recovery_strategy {
        JsonlRecoveryStrategy::RebuildFromJsonl => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                anomaly = %anomaly,
                "Detected recoverable database anomaly after open; rebuilding from JSONL"
            );
            // Snapshot tombstones from the anomalous (but still queryable)
            // storage BEFORE we drop it. The anomaly detector only flags
            // duplicates on `blocked_issues_cache` / its index and on
            // `config`/`metadata` kv duplicates — none of which break a
            // plain `SELECT … FROM issues`. Without this snapshot, any
            // unflushed local tombstones that haven't landed in JSONL
            // would be silently dropped by the rebuild (which only
            // imports what's in JSONL), taking their deletion-retention
            // state with them.
            let (storage, recovered_jsonl) = rebuild_with_tombstone_preservation(
                storage,
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                write_authority,
            )?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: true,
                pending_recovery_backup: None,
                recovered_jsonl: Some(recovered_jsonl),
            })
        }
        JsonlRecoveryStrategy::DeferToExplicitImport => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                anomaly = %anomaly,
                "Detected recoverable database anomaly after open; deferring JSONL recovery to explicit import semantics"
            );
            drop(storage);
            let (storage, backup_set) = prepare_fresh_storage()?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: false,
                pending_recovery_backup: Some(backup_set),
                recovered_jsonl: None,
            })
        }
    }
}

/// Handle the `Err(probe_err)` branch from
/// `detect_recoverable_open_anomaly`. Counterpart of
/// `rebuild_or_defer_after_recoverable_anomaly`; split out for the same
/// reason and sharing the same preservation helper so both anomaly
/// surfaces rescue unflushed tombstones consistently.
#[allow(clippy::too_many_arguments)]
fn rebuild_or_defer_after_probe_error(
    storage: SqliteStorage,
    probe_err: &BeadsError,
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    recovery_strategy: JsonlRecoveryStrategy,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
    prepare_fresh_storage: &dyn Fn() -> Result<(SqliteStorage, RecoveryBackupSet)>,
) -> Result<SqliteRecoveryOpenResult> {
    match recovery_strategy {
        JsonlRecoveryStrategy::RebuildFromJsonl => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                probe_error = %probe_err,
                "Post-open database probe failed; rebuilding from JSONL"
            );
            // Best-effort tombstone + dirty-issue snapshot. If the probe
            // failed the storage may be in a strange state, but the
            // snapshot helpers are fault-tolerant (warn+empty on
            // enumeration failure, warn+partial on per-issue failure), so
            // we try anyway: the worst case is the same as the old
            // behavior (no preservation), the best case is we rescue the
            // unflushed tombstones and unflushed local edits the old code
            // lost silently (GitHub #394).
            let (storage, recovered_jsonl) = rebuild_with_tombstone_preservation(
                storage,
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                allow_external_jsonl,
                write_authority,
            )?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: true,
                pending_recovery_backup: None,
                recovered_jsonl: Some(recovered_jsonl),
            })
        }
        JsonlRecoveryStrategy::DeferToExplicitImport => {
            warn!(
                db_path = %paths.db_path.display(),
                jsonl_path = %paths.jsonl_path.display(),
                probe_error = %probe_err,
                "Post-open database probe failed; deferring JSONL recovery to explicit import semantics"
            );
            drop(storage);
            let (storage, backup_set) = prepare_fresh_storage()?;
            Ok(SqliteRecoveryOpenResult {
                storage,
                auto_rebuilt: false,
                pending_recovery_backup: Some(backup_set),
                recovered_jsonl: None,
            })
        }
    }
}

/// Snapshot unflushed tombstones and dirty live issues from `storage`,
/// drop the old connection, rebuild from JSONL, and restore the preserved
/// state atomically. Without the dirty-issue half, an automatic rebuild
/// would silently drop any local creation or edit that had not yet been
/// flushed to the JSONL (GitHub #394).
fn rebuild_with_tombstone_preservation(
    storage: SqliteStorage,
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<(SqliteStorage, RecoveredJsonlSource)> {
    let write_authority = write_authority.ok_or_else(|| BeadsError::SyncConflict {
        message: "Database recovery has no database-family authority".to_string(),
    })?;
    let import_config = import_config_for_resolved_jsonl(
        beads_dir,
        &paths.db_path,
        &paths.jsonl_path,
        allow_external_jsonl,
    );
    validate_sync_path_with_external(
        &paths.jsonl_path,
        beads_dir,
        import_config.allow_external_jsonl,
    )?;
    let jsonl_authority = Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
        &paths.jsonl_path,
        lock_timeout,
    )?);
    jsonl_authority.verify_jsonl_authority()?;
    let source = Arc::new(crate::sync::capture_jsonl_source_snapshot(
        &paths.jsonl_path,
    )?);
    let (preserved_tombstones, preserved_dirty_issues) =
        preserved_unflushed_state(&storage, &source);
    drop(storage);
    let mut storage = rebuild_database_from_jsonl_snapshot(
        beads_dir,
        paths,
        lock_timeout,
        bootstrap_layer,
        import_config,
        &source,
        &jsonl_authority,
        write_authority,
    )?;
    restore_tombstones_after_rebuild(&mut storage, &preserved_tombstones)?;
    restore_dirty_issues_after_rebuild(&mut storage, &preserved_dirty_issues)?;
    Ok((
        storage,
        RecoveredJsonlSource {
            source,
            authority: jsonl_authority,
        },
    ))
}

fn rebuild_database_from_jsonl(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    allow_external_jsonl: bool,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
) -> Result<(SqliteStorage, RecoveredJsonlSource)> {
    let write_authority = write_authority.ok_or_else(|| BeadsError::SyncConflict {
        message: "Database recovery has no database-family authority".to_string(),
    })?;
    let import_config = import_config_for_resolved_jsonl(
        beads_dir,
        &paths.db_path,
        &paths.jsonl_path,
        allow_external_jsonl,
    );
    validate_sync_path_with_external(
        &paths.jsonl_path,
        beads_dir,
        import_config.allow_external_jsonl,
    )?;
    let jsonl_authority = Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
        &paths.jsonl_path,
        lock_timeout,
    )?);
    jsonl_authority.verify_jsonl_authority()?;
    let source = Arc::new(crate::sync::capture_jsonl_source_snapshot(
        &paths.jsonl_path,
    )?);
    let storage = rebuild_database_from_jsonl_snapshot(
        beads_dir,
        paths,
        lock_timeout,
        bootstrap_layer,
        import_config,
        &source,
        &jsonl_authority,
        write_authority,
    )?;
    Ok((
        storage,
        RecoveredJsonlSource {
            source,
            authority: jsonl_authority,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn rebuild_database_from_jsonl_snapshot(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    import_config: ImportConfig,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<SqliteStorage> {
    repair_database_from_jsonl_snapshot_with_import_config_under_write_authority(
        beads_dir,
        &paths.db_path,
        lock_timeout,
        bootstrap_layer,
        import_config,
        source,
        jsonl_authority,
        write_authority,
    )
    .map(|(storage, _, _)| storage)
}

/// Snapshot local tombstones and dirty live issues that have not yet been
/// flushed to JSONL.
///
/// Returns empty vectors and logs a debug/warn entry on any failure —
/// this is a preservation path, not a correctness invariant, so we always
/// prefer to proceed with the rebuild rather than fail the whole command
/// because we couldn't read an issue or couldn't read the JSONL. The
/// returned vectors are filtered so only state the JSONL cannot reproduce
/// survives: tombstones not already flushed as tombstones, and dirty live
/// issues whose row is absent from the JSONL or strictly newer than its
/// JSONL copy (everything else comes back via the rebuild's own
/// `import_from_jsonl`).
fn preserved_unflushed_state(
    storage: &SqliteStorage,
    source: &JsonlSourceSnapshot,
) -> (Vec<PreservedIssue>, Vec<PreservedIssue>) {
    let tombstone_snapshot = snapshot_tombstones(storage);
    let dirty_snapshot = snapshot_dirty_live_issues(storage);
    if tombstone_snapshot.is_empty() && dirty_snapshot.is_empty() {
        return (tombstone_snapshot, dirty_snapshot);
    }
    let jsonl_filter = match scan_jsonl_snapshot_for_tombstone_filter(source) {
        Ok(filter) => filter,
        Err(err) => {
            // The rebuild itself will re-surface the same immutable source
            // error with a full diagnostic. For the purposes of preservation,
            // fall back to treating the JSONL as empty so every snapshotted
            // issue is kept; if the rebuild ultimately fails, the backup set
            // has our back.
            tracing::debug!(
                error = %err,
                "Could not scan immutable JSONL snapshot for tombstone filter during startup auto-rebuild; preserving all snapshotted state and letting the rebuild surface the JSONL error"
            );
            JsonlTombstoneFilter::default()
        }
    };
    (
        tombstones_missing_from_jsonl_tombstones(tombstone_snapshot, &jsonl_filter),
        dirty_issues_missing_from_jsonl(dirty_snapshot, &jsonl_filter),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn repair_database_from_jsonl_snapshot(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    show_progress: bool,
    allow_external_jsonl: bool,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
) -> Result<(SqliteStorage, ImportResult, Vec<RecoveryBackupVerification>)> {
    let write_authority = Arc::new(blocking_database_family_write_lock_with_timeout(
        beads_dir,
        db_path,
        lock_timeout,
    )?);
    repair_database_from_jsonl_snapshot_under_write_authority(
        beads_dir,
        db_path,
        lock_timeout,
        bootstrap_layer,
        show_progress,
        allow_external_jsonl,
        source,
        jsonl_authority,
        &write_authority,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn repair_database_from_jsonl_snapshot_under_write_authority(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    show_progress: bool,
    allow_external_jsonl: bool,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<(SqliteStorage, ImportResult, Vec<RecoveryBackupVerification>)> {
    let mut import_config = import_config_for_resolved_jsonl(
        beads_dir,
        db_path,
        source.display_path(),
        allow_external_jsonl,
    );
    import_config.show_progress = show_progress;
    import_config.skip_prefix_validation = true;

    repair_database_from_jsonl_snapshot_with_import_config_under_write_authority(
        beads_dir,
        db_path,
        lock_timeout,
        bootstrap_layer,
        import_config,
        source,
        jsonl_authority,
        write_authority,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn repair_database_from_jsonl_snapshot_with_import_config(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    import_config: ImportConfig,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
) -> Result<(SqliteStorage, ImportResult, Vec<RecoveryBackupVerification>)> {
    let write_authority = Arc::new(blocking_database_family_write_lock_with_timeout(
        beads_dir,
        db_path,
        lock_timeout,
    )?);
    repair_database_from_jsonl_snapshot_with_import_config_under_write_authority(
        beads_dir,
        db_path,
        lock_timeout,
        bootstrap_layer,
        import_config,
        source,
        jsonl_authority,
        &write_authority,
    )
}

#[allow(clippy::too_many_arguments)]
// Takes `ImportConfig` by value for external callers (cli/commands/sync.rs);
// the internal impl only borrows it.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn repair_database_from_jsonl_snapshot_with_import_config_under_write_authority(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    import_config: ImportConfig,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<(SqliteStorage, ImportResult, Vec<RecoveryBackupVerification>)> {
    let (storage, import_result, backup_set) =
        repair_database_from_jsonl_snapshot_with_import_config_under_write_authority_impl(
            beads_dir,
            db_path,
            lock_timeout,
            bootstrap_layer,
            &import_config,
            source,
            jsonl_authority,
            write_authority,
            SuccessfulRecoveryDisposition::FinalizeImmediately,
        )?;
    Ok((storage, import_result, backup_set.verified_files))
}

#[allow(clippy::too_many_arguments)]
fn repair_database_from_jsonl_snapshot_with_import_config_deferred(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    import_config: &ImportConfig,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<(SqliteStorage, ImportResult, RecoveryBackupSet)> {
    repair_database_from_jsonl_snapshot_with_import_config_under_write_authority_impl(
        beads_dir,
        db_path,
        lock_timeout,
        bootstrap_layer,
        import_config,
        source,
        jsonl_authority,
        write_authority,
        SuccessfulRecoveryDisposition::RetainBackupUntilCommandSuccess,
    )
}

#[allow(clippy::too_many_arguments)]
fn repair_database_from_jsonl_snapshot_with_import_config_under_write_authority_impl(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    import_config: &ImportConfig,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    successful_disposition: SuccessfulRecoveryDisposition,
) -> Result<(SqliteStorage, ImportResult, RecoveryBackupSet)> {
    write_authority.verify_database_authority()?;
    jsonl_authority.verify_jsonl_authority()?;
    let prefix = resolve_bootstrap_issue_prefix_snapshot(bootstrap_layer, beads_dir, source)?;

    let mut preflight_config = import_config.clone();
    preflight_config.skip_prefix_validation = true;
    preflight_import_snapshot(source, &preflight_config, Some(&prefix))?.into_result()?;
    let current_source = crate::sync::capture_jsonl_source_snapshot(source.display_path())?;
    if current_source.state_witness() != source.state_witness() {
        return Err(BeadsError::SyncConflict {
            message: "JSONL changed after its recovery snapshot was captured; database recovery was not started"
                .to_string(),
        });
    }

    warn!(
        db_path = %db_path.display(),
        jsonl_path = %source.display_path().display(),
        "Rebuilding SQLite database from JSONL"
    );

    let ((mut storage, import_result), backup_set) = rebuild_database_family_with_backup(
        db_path,
        beads_dir,
        write_authority,
        successful_disposition,
        |fresh_witness| {
            let rebuilt = rebuild_database_family(
                db_path,
                lock_timeout,
                source,
                import_config,
                &prefix,
                write_authority,
                fresh_witness,
            )?;
            jsonl_authority.verify_jsonl_authority()?;
            let current_source = crate::sync::capture_jsonl_source_snapshot(source.display_path())?;
            if current_source.state_witness() != source.state_witness() {
                return Err(BeadsError::SyncConflict {
                    message: "JSONL changed during database recovery; restored the original database family"
                        .to_string(),
                });
            }
            Ok(rebuilt)
        },
    )?;
    write_authority.verify_database_authority()?;
    storage.attach_write_authority(Arc::clone(write_authority));
    let recovery_dir = backup_set.recovery_dir.clone();
    let verified_backups = &backup_set.verified_files;

    warn!(
        db_path = %db_path.display(),
        recovery_dir = %recovery_dir.display(),
        verified_backup_count = verified_backups.len(),
        verified_backups = ?verified_backups,
        "SQLite rebuild from JSONL succeeded"
    );
    Ok((storage, import_result, backup_set))
}

fn should_surface_recovery_error(recovery_err: &BeadsError) -> bool {
    matches!(recovery_err, BeadsError::WithContext { .. })
        || is_symlinked_database_recovery_error(recovery_err)
}

fn is_symlinked_database_recovery_error(error: &BeadsError) -> bool {
    matches!(
        error,
        BeadsError::Config(message)
            if message.starts_with(SYMLINKED_DB_RECOVERY_ERROR_PREFIX)
    )
}

fn reject_symlinked_database_path_for_recovery(db_path: &Path) -> Result<()> {
    crate::sync::reject_symlinked_database_route_components(db_path)?;
    match fs::symlink_metadata(db_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(BeadsError::Config(format!(
            "{SYMLINKED_DB_RECOVERY_ERROR_PREFIX} '{}'; replace the symlink with a regular database file or run recovery against the resolved target explicitly",
            db_path.display()
        ))),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BeadsError::WithContext {
            context: format!(
                "Failed to inspect database path '{}' before recovery",
                db_path.display()
            ),
            source: Box::new(err),
        }),
    }
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

fn rollback_renamed_paths(renamed_paths: &[RecoveryBackupPath], operation: &str) -> Result<()> {
    let mut first_sync_error = None;
    for (original, renamed) in renamed_paths.iter().rev() {
        rename_recovery_path_no_replace(renamed, original).map_err(|error| {
            BeadsError::WithContext {
                context: format!(
                    "Failed to roll back {operation}: restore '{}' from '{}' without replacing an existing path",
                    original.display(),
                    renamed.display()
                ),
                source: Box::new(error),
            }
        })?;
        if let Err(error) = crate::util::sync_rename_parent_directories(renamed, original)
            && first_sync_error.is_none()
        {
            first_sync_error = Some(BeadsError::WithContext {
                context: format!(
                    "Rolled back {operation}, but failed to make the restored namespace durable"
                ),
                source: Box::new(error),
            });
        }
    }

    first_sync_error.map_or(Ok(()), Err)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| {
        format!(
            "Failed to open recovery artifact '{}' for hashing",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let read = file.read(&mut buffer).with_context(|| {
            format!(
                "Failed to read recovery artifact '{}' for hashing",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex_encode(&hasher.finalize()))
}

fn recovery_artifact_fingerprint(path: &Path) -> Result<RecoveryArtifactFingerprint> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "Failed to inspect recovery artifact '{}' for verification",
            path.display()
        )
    })?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        let target = fs::read_link(path).with_context(|| {
            format!(
                "Failed to read recovery symlink artifact '{}'",
                path.display()
            )
        })?;
        return Ok(RecoveryArtifactFingerprint {
            kind: "symlink".to_string(),
            size_bytes: None,
            sha256: None,
            symlink_target: Some(target.display().to_string()),
        });
    }

    if metadata.is_file() {
        return Ok(RecoveryArtifactFingerprint {
            kind: "file".to_string(),
            size_bytes: Some(metadata.len()),
            sha256: Some(sha256_file(path)?),
            symlink_target: None,
        });
    }

    if metadata.is_dir() {
        return Ok(RecoveryArtifactFingerprint {
            kind: "directory".to_string(),
            size_bytes: None,
            sha256: None,
            symlink_target: None,
        });
    }

    Ok(RecoveryArtifactFingerprint {
        kind: "other".to_string(),
        size_bytes: None,
        sha256: None,
        symlink_target: None,
    })
}

fn capture_live_database_family_witness(db_path: &Path) -> Result<Vec<RecoveryLivePathWitness>> {
    database_family_paths(db_path)
        .into_iter()
        .map(|original| {
            let fingerprint = match fs::symlink_metadata(&original) {
                Ok(_) => Some(recovery_artifact_fingerprint(&original)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Failed to witness live database-family path '{}' before recovery rollback",
                            original.display()
                        ),
                        source: Box::new(error),
                    });
                }
            };
            Ok(RecoveryLivePathWitness {
                original,
                fingerprint,
            })
        })
        .collect()
}

fn verify_staged_database_family_witness(
    witnesses: &[RecoveryLivePathWitness],
    staged_paths: &[RecoveryBackupPath],
) -> Result<()> {
    if staged_paths.iter().any(|(original, _)| {
        !witnesses
            .iter()
            .any(|witness| witness.original == *original)
    }) {
        return Err(BeadsError::SyncConflict {
            message:
                "Recovery staged an unexpected database-family path after its final authority check"
                    .to_string(),
        });
    }

    for witness in witnesses {
        let staged_path = staged_paths
            .iter()
            .find_map(|(original, staged)| (original == &witness.original).then_some(staged));
        match (&witness.fingerprint, staged_path) {
            (None, None) => {}
            (Some(expected), Some(staged)) => {
                verify_recovery_backup_artifact(staged, expected).map_err(|error| {
                    BeadsError::WithContext {
                        context: format!(
                            "Live database-family path '{}' changed between its final recovery witness and staging; refusing to install the original backup",
                            witness.original.display()
                        ),
                        source: Box::new(error),
                    }
                })?;
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Live database-family path '{}' appeared or disappeared between its final recovery witness and staging; refusing to install the original backup",
                        witness.original.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn verify_recovery_backup_artifact(
    backup: &Path,
    expected: &RecoveryArtifactFingerprint,
) -> Result<()> {
    let actual = recovery_artifact_fingerprint(backup)?;
    if &actual == expected {
        return Ok(());
    }

    Err(BeadsError::Config(format!(
        "Recovery backup verification failed for '{}': expected {:?}, found {:?}",
        backup.display(),
        expected,
        actual
    )))
}

fn recovery_backup_verification(
    original: &Path,
    backup: &Path,
    fingerprint: &RecoveryArtifactFingerprint,
) -> RecoveryBackupVerification {
    RecoveryBackupVerification {
        original: original.display().to_string(),
        backup: backup.display().to_string(),
        kind: fingerprint.kind.clone(),
        size_bytes: fingerprint.size_bytes,
        sha256: fingerprint.sha256.clone(),
        symlink_target: fingerprint.symlink_target.clone(),
    }
}

fn expected_recovery_artifact_fingerprint(
    verification: &RecoveryBackupVerification,
) -> RecoveryArtifactFingerprint {
    RecoveryArtifactFingerprint {
        kind: verification.kind.clone(),
        size_bytes: verification.size_bytes,
        sha256: verification.sha256.clone(),
        symlink_target: verification.symlink_target.clone(),
    }
}

fn verify_recovery_backup_manifest(backup_set: &RecoveryBackupSet) -> Result<()> {
    if backup_set.files.len() != backup_set.verified_files.len() {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Verified backup manifest in '{}' is incomplete",
                backup_set.recovery_dir.display()
            ),
        });
    }
    for ((original, backup), verification) in
        backup_set.files.iter().zip(&backup_set.verified_files)
    {
        if verification.original != original.display().to_string()
            || verification.backup != backup.display().to_string()
        {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Verified backup manifest in '{}' does not match its recovery paths",
                    backup_set.recovery_dir.display()
                ),
            });
        }
    }
    Ok(())
}

fn verify_recovery_backup_set(backup_set: &RecoveryBackupSet) -> Result<()> {
    verify_recovery_backup_manifest(backup_set)?;
    for ((_, backup), verification) in backup_set.files.iter().zip(&backup_set.verified_files) {
        verify_recovery_backup_artifact(
            backup,
            &expected_recovery_artifact_fingerprint(verification),
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingRenameSourcePolicy {
    Skip,
    Error,
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_recovery_path_no_replace(from: &Path, to: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => Err(BeadsError::SyncConflict {
            message: format!(
                "Refusing to replace recovery namespace target '{}' while moving '{}'",
                to.display(),
                from.display()
            ),
        }),
        Err(error) => Err(BeadsError::Io(std::io::Error::from(error))),
    }
}

#[cfg(windows)]
fn rename_recovery_path_no_replace(from: &Path, to: &Path) -> Result<()> {
    match crate::sync::rename_path_no_replace_windows(from, to) {
        Ok(()) => Ok(()),
        Err(error) => match fs::symlink_metadata(to) {
            Ok(_) => Err(BeadsError::SyncConflict {
                message: format!(
                    "Refusing to replace recovery namespace target '{}' while moving '{}'",
                    to.display(),
                    from.display()
                ),
            }),
            Err(inspect_error) if inspect_error.kind() == std::io::ErrorKind::NotFound => {
                Err(BeadsError::Io(error))
            }
            Err(inspect_error) => Err(BeadsError::WithContext {
                context: format!(
                    "Failed to inspect recovery namespace target '{}' after rename failed",
                    to.display()
                ),
                source: Box::new(inspect_error),
            }),
        },
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_recovery_path_no_replace(_from: &Path, _to: &Path) -> Result<()> {
    Err(BeadsError::Config(
        "This platform does not provide the atomic no-replace primitive required for database recovery restore"
            .to_string(),
    ))
}

fn rollback_renamed_paths_no_replace(
    renamed_paths: &[RecoveryBackupPath],
    operation: &str,
) -> Result<()> {
    let mut first_sync_error = None;
    for (original, renamed) in renamed_paths.iter().rev() {
        rename_recovery_path_no_replace(renamed, original).map_err(|error| {
            BeadsError::WithContext {
                context: format!(
                    "Failed to roll back {operation}: restore '{}' from '{}' without replacing an existing path",
                    original.display(),
                    renamed.display()
                ),
                source: Box::new(error),
            }
        })?;
        if let Err(error) = crate::util::sync_rename_parent_directories(renamed, original)
            && first_sync_error.is_none()
        {
            first_sync_error = Some(BeadsError::WithContext {
                context: format!(
                    "Rolled back {operation}, but failed to make the restored namespace durable"
                ),
                source: Box::new(error),
            });
        }
    }

    first_sync_error.map_or(Ok(()), Err)
}

fn rename_existing_paths_no_replace<I>(
    paths: I,
    operation: &str,
    missing_source_policy: MissingRenameSourcePolicy,
) -> Result<Vec<RecoveryBackupPath>>
where
    I: IntoIterator<Item = (PathBuf, PathBuf)>,
{
    let mut renamed_paths = Vec::new();

    for (original, renamed) in paths {
        match fs::symlink_metadata(&original) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if matches!(missing_source_policy, MissingRenameSourcePolicy::Skip) {
                    continue;
                }
                let rename_error = BeadsError::WithContext {
                    context: format!("Failed to {operation}"),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("expected '{}' to exist", original.display()),
                    )),
                };
                if let Err(rollback_error) =
                    rollback_renamed_paths_no_replace(&renamed_paths, operation)
                {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Failed to {operation} ({rename_error}); rollback also failed"
                        ),
                        source: Box::new(rollback_error),
                    });
                }
                return Err(rename_error);
            }
            Err(error) => {
                let rename_error = BeadsError::WithContext {
                    context: format!(
                        "Failed to inspect '{}' before attempting to {operation}",
                        original.display()
                    ),
                    source: Box::new(error),
                };
                if let Err(rollback_error) =
                    rollback_renamed_paths_no_replace(&renamed_paths, operation)
                {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Failed to {operation} ({rename_error}); rollback also failed"
                        ),
                        source: Box::new(rollback_error),
                    });
                }
                return Err(rename_error);
            }
        }

        if let Err(rename_error) = rename_recovery_path_no_replace(&original, &renamed) {
            if let Err(rollback_error) =
                rollback_renamed_paths_no_replace(&renamed_paths, operation)
            {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Failed to {operation} ({rename_error}); rollback also failed"
                    ),
                    source: Box::new(rollback_error),
                });
            }
            return Err(rename_error);
        }

        renamed_paths.push((original.clone(), renamed.clone()));
        if let Err(sync_error) = crate::util::sync_rename_parent_directories(&original, &renamed) {
            if let Err(rollback_error) =
                rollback_renamed_paths_no_replace(&renamed_paths, operation)
            {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Completed a namespace rename while attempting to {operation}, but parent-directory sync failed ({sync_error}); rollback also failed"
                    ),
                    source: Box::new(rollback_error),
                });
            }
            return Err(BeadsError::WithContext {
                context: format!(
                    "Completed and rolled back a namespace rename while attempting to {operation}, but parent-directory sync failed"
                ),
                source: Box::new(sync_error),
            });
        }
    }

    Ok(renamed_paths)
}

#[allow(clippy::too_many_lines)]
fn rename_existing_paths_with_backup_verification<I>(
    paths: I,
    operation: &str,
    missing_source_policy: MissingRenameSourcePolicy,
) -> Result<VerifiedRecoveryBackupBatch>
where
    I: IntoIterator<Item = (PathBuf, PathBuf)>,
{
    let mut renamed_paths = Vec::new();
    let mut verified_files = Vec::new();

    for (original, renamed) in paths {
        let fingerprint = match fs::symlink_metadata(&original) {
            Ok(_) => recovery_artifact_fingerprint(&original)?,
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
        };

        if let Err(rename_err) = rename_recovery_path_no_replace(&original, &renamed) {
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

            return Err(rename_err);
        }

        renamed_paths.push((original.clone(), renamed.clone()));
        if let Err(sync_err) = crate::util::sync_rename_parent_directories(&original, &renamed) {
            if let Err(rollback_err) = rollback_renamed_paths(&renamed_paths, operation) {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Completed a namespace rename while attempting to {operation}, but parent-directory sync failed ({sync_err}); rollback also failed"
                    ),
                    source: Box::new(rollback_err),
                });
            }
            return Err(BeadsError::WithContext {
                context: format!(
                    "Completed and rolled back a namespace rename while attempting to {operation}, but parent-directory sync failed"
                ),
                source: Box::new(sync_err),
            });
        }
        if let Err(verify_err) = verify_recovery_backup_artifact(&renamed, &fingerprint) {
            if let Err(rollback_err) = rollback_renamed_paths(&renamed_paths, operation) {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Failed to verify recovery backup '{}' after {operation} ({verify_err}); rollback also failed",
                        renamed.display()
                    ),
                    source: Box::new(rollback_err),
                });
            }

            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to verify recovery backup '{}' after {operation}",
                    renamed.display()
                ),
                source: Box::new(verify_err),
            });
        }
        verified_files.push(recovery_backup_verification(
            &original,
            &renamed,
            &fingerprint,
        ));
    }

    Ok((renamed_paths, verified_files))
}

fn rebuild_database_family(
    db_path: &Path,
    lock_timeout: Option<u64>,
    source: &JsonlSourceSnapshot,
    import_config: &ImportConfig,
    prefix: &str,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    fresh_witness: FreshDatabaseReplacementWitness,
) -> Result<(SqliteStorage, ImportResult)> {
    write_authority.verify_database_authority()?;
    let mut storage = SqliteStorage::open_with_timeout_under_write_authority(
        db_path,
        lock_timeout,
        write_authority,
    )?;
    write_authority.verify_database_authority()?;
    storage.set_config("issue_prefix", prefix)?;
    let import_result = import_from_jsonl_snapshot_into_fresh_replacement(
        &mut storage,
        source,
        import_config,
        Some(prefix),
        fresh_witness,
    )?;

    // Drain the WAL to the main DB file so the follow-up maintenance (VACUUM,
    // REINDEX, VACUUM INTO) operates against what is actually on disk.
    // Without this, fsqlite's post-import MVCC state can lag behind and
    // maintenance silently fails with "database is busy (snapshot conflict
    // on pages: page N > snapshot db_size M)", leaving the corruption it
    // was supposed to clean up in place.
    if let Err(e) = storage.checkpoint_full() {
        tracing::warn!(
            error = %e,
            db_path = %db_path.display(),
            "Full WAL checkpoint after rebuild failed (non-fatal)"
        );
    }

    // Post-rebuild VACUUM to eliminate freeblock accounting anomalies that
    // frankensqlite's B-tree layer may leave behind during bulk import.
    // Without this, C sqlite3's `PRAGMA integrity_check` can report
    // "free space corruption" even though the data is intact (issue #237).
    if let Err(e) = storage.execute_raw("VACUUM") {
        tracing::warn!(
            error = %e,
            db_path = %db_path.display(),
            "VACUUM after rebuild failed (non-fatal); on-disk DB may still contain free-space corruption"
        );
    }

    // Post-rebuild REINDEX to fix partial-index row mismatches that
    // frankensqlite's B-tree layer can introduce during bulk insert.
    // VACUUM rewrites pages but does not rebuild index entries; without
    // REINDEX, `PRAGMA integrity_check` reports "row N missing from index"
    // for partial indexes like idx_issues_list_active_order (issue #246).
    if let Err(e) = storage.execute_raw("REINDEX") {
        tracing::warn!(
            error = %e,
            db_path = %db_path.display(),
            "REINDEX after rebuild failed (non-fatal); partial-index entries may be inconsistent"
        );
    }

    // Compact the rebuilt DB via `VACUUM INTO` and atomic rename. This is
    // the only reliable way to make upstream sqlite3's
    // `PRAGMA integrity_check` report `ok` on a file produced by fsqlite's
    // bulk-insert + REINDEX path (issue #248). In-place VACUUM alone —
    // even called twice after a fresh checkpoint — does not truncate the
    // trailing pages that fsqlite's REINDEX leaves orphaned: those pages
    // exist in the file but are neither on the freelist nor referenced
    // from any B-tree root. `VACUUM INTO` sidesteps fsqlite's in-place
    // truncation bug because it writes a brand-new compacted file from
    // the reachable page set — page count matches exactly what sqlite3's
    // own `VACUUM INTO` produces. The subsequent atomic rename is
    // crash-safe on POSIX (within a filesystem) and keeps the database
    // family consistent with its sidecars, which we drop first.
    storage = compact_database_via_vacuum_into_in_place_under_write_authority(
        storage,
        db_path,
        lock_timeout,
        write_authority,
    )?;
    write_authority.verify_database_authority()?;
    verify_rebuilt_database_postconditions(&storage, &import_result)?;
    Ok((storage, import_result))
}

fn verify_rebuilt_database_postconditions(
    storage: &SqliteStorage,
    import_result: &ImportResult,
) -> Result<()> {
    let issue_count = storage
        .count_issues()
        .map_err(|source| BeadsError::WithContext {
            context: "Post-recovery validation failed while counting rebuilt issues".to_string(),
            source: Box::new(source),
        })?;
    if issue_count != import_result.created_count {
        return Err(BeadsError::Config(format!(
            "post-recovery validation failed: JSONL import created {} issue rows, but rebuilt database contains {issue_count}",
            import_result.created_count
        )));
    }

    let missing_references =
        storage
            .missing_issue_references()
            .map_err(|source| BeadsError::WithContext {
                context: "Post-recovery validation failed while checking issue references"
                    .to_string(),
                source: Box::new(source),
            })?;
    if !missing_references.is_empty() {
        return Err(BeadsError::Config(format!(
            "post-recovery validation failed: rebuilt database contains orphaned issue references in {}",
            missing_references.join(", ")
        )));
    }

    verify_rebuilt_table_count(
        storage,
        "labels",
        import_result.labels_imported,
        "labels imported from JSONL",
    )?;
    verify_rebuilt_table_count(
        storage,
        "dependencies",
        import_result.dependencies_imported,
        "dependencies imported from JSONL",
    )?;
    verify_rebuilt_table_count(
        storage,
        "comments",
        import_result.comments_imported,
        "comments imported from JSONL",
    )?;
    verify_rebuilt_table_count(
        storage,
        "events",
        0,
        "events are local-only and not in JSONL",
    )?;
    verify_rebuilt_table_count(
        storage,
        "dirty_issues",
        0,
        "dirty markers should be absent immediately after JSONL rebuild",
    )?;
    verify_rebuilt_table_count(
        storage,
        "export_hashes",
        import_result.export_hashes_recorded,
        "export hashes recorded for imported JSONL",
    )?;
    verify_rebuilt_table_count(
        storage,
        "blocked_issues_cache",
        import_result.blocked_cache_entries,
        "blocked cache rows rebuilt after import",
    )?;
    verify_blocked_cache_payloads(storage)?;
    verify_child_counters(storage, import_result.child_counter_entries)?;

    Ok(())
}

fn verify_rebuilt_table_count(
    storage: &SqliteStorage,
    table: &str,
    expected: usize,
    invariant: &str,
) -> Result<()> {
    let actual = count_recovery_table_rows(storage, table)?;
    if actual == expected {
        return Ok(());
    }

    Err(BeadsError::Config(format!(
        "post-recovery validation failed: {table} row count mismatch ({invariant}); expected {expected}, found {actual}"
    )))
}

fn count_recovery_table_rows(storage: &SqliteStorage, table: &str) -> Result<usize> {
    const ALLOWED_TABLES: &[&str] = &[
        "labels",
        "dependencies",
        "comments",
        "events",
        "dirty_issues",
        "export_hashes",
        "blocked_issues_cache",
        "child_counters",
    ];
    if !ALLOWED_TABLES.contains(&table) {
        return Err(BeadsError::Config(format!(
            "post-recovery validation refused disallowed table count for {table}"
        )));
    }

    let rows = storage
        .execute_raw_query(&format!("SELECT COUNT(*) FROM {table}"))
        .map_err(|source| BeadsError::WithContext {
            context: format!("Post-recovery validation failed while counting {table} rows"),
            source: Box::new(source),
        })?;
    let count = rows
        .first()
        .and_then(|row| row.first())
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);
    usize::try_from(count).map_err(|_| {
        BeadsError::Config(format!(
            "post-recovery validation failed: {table} row count is negative ({count})"
        ))
    })
}

fn verify_blocked_cache_payloads(storage: &SqliteStorage) -> Result<()> {
    let rows = storage
        .execute_raw_query("SELECT issue_id, blocked_by FROM blocked_issues_cache")
        .map_err(|source| BeadsError::WithContext {
            context: "Post-recovery validation failed while reading blocked_issues_cache"
                .to_string(),
            source: Box::new(source),
        })?;

    for row in rows {
        let issue_id = row
            .first()
            .and_then(SqliteValue::as_text)
            .unwrap_or("<missing>");
        let blocked_by = row.get(1).and_then(SqliteValue::as_text).ok_or_else(|| {
            BeadsError::Config(format!(
                "post-recovery validation failed: blocked_issues_cache.blocked_by missing for {issue_id}"
            ))
        })?;
        let blockers: Vec<String> = serde_json::from_str(blocked_by).map_err(|err| {
            BeadsError::Config(format!(
                "post-recovery validation failed: blocked_issues_cache.blocked_by contains invalid JSON for {issue_id}: {err}"
            ))
        })?;
        if blockers.is_empty() {
            return Err(BeadsError::Config(format!(
                "post-recovery validation failed: blocked_issues_cache contains empty blocker list for {issue_id}"
            )));
        }
    }

    Ok(())
}

fn verify_child_counters(storage: &SqliteStorage, rebuilt_count: usize) -> Result<()> {
    verify_rebuilt_table_count(
        storage,
        "child_counters",
        rebuilt_count,
        "child counters rebuilt after import",
    )?;

    let expected = expected_child_counters(storage)?;
    let actual = actual_child_counters(storage)?;
    if actual == expected {
        return Ok(());
    }

    Err(BeadsError::Config(format!(
        "post-recovery validation failed: child_counters derived values differ from rebuilt issue IDs; expected {expected:?}, found {actual:?}"
    )))
}

fn expected_child_counters(storage: &SqliteStorage) -> Result<HashMap<String, u32>> {
    let rows = storage
        .execute_raw_query("SELECT id FROM issues")
        .map_err(|source| BeadsError::WithContext {
            context: "Post-recovery validation failed while reading issue IDs".to_string(),
            source: Box::new(source),
        })?;
    let issue_ids: HashSet<String> = rows
        .iter()
        .filter_map(|row| {
            row.first()
                .and_then(SqliteValue::as_text)
                .map(str::to_string)
        })
        .collect();
    let mut expected = HashMap::new();

    for id in &issue_ids {
        let Ok(parsed) = parse_id(id) else {
            continue;
        };
        if parsed.is_root() {
            continue;
        }
        let Some(parent) = parsed.parent() else {
            continue;
        };
        if !issue_ids.contains(&parent) {
            continue;
        }
        let Some(&child_number) = parsed.child_path.last() else {
            continue;
        };
        let entry = expected.entry(parent).or_insert(0);
        if child_number > *entry {
            *entry = child_number;
        }
    }

    Ok(expected)
}

fn actual_child_counters(storage: &SqliteStorage) -> Result<HashMap<String, u32>> {
    let rows = storage
        .execute_raw_query("SELECT parent_id, last_child FROM child_counters")
        .map_err(|source| BeadsError::WithContext {
            context: "Post-recovery validation failed while reading child_counters".to_string(),
            source: Box::new(source),
        })?;
    let mut actual = HashMap::new();

    for row in rows {
        let parent_id = row
            .first()
            .and_then(SqliteValue::as_text)
            .ok_or_else(|| {
                BeadsError::Config(
                    "post-recovery validation failed: child_counters.parent_id missing".to_string(),
                )
            })?
            .to_string();
        let last_child = row.get(1).and_then(SqliteValue::as_integer).ok_or_else(|| {
            BeadsError::Config(format!(
                "post-recovery validation failed: child_counters.last_child missing for {parent_id}"
            ))
        })?;
        let last_child = u32::try_from(last_child).map_err(|_| {
            BeadsError::Config(format!(
                "post-recovery validation failed: child_counters.last_child is invalid for {parent_id}: {last_child}"
            ))
        })?;
        actual.insert(parent_id, last_child);
    }

    Ok(actual)
}

/// Engine-managed sidecar files that live alongside a database file.
///
/// `-fsqlite-ns-gate` / `-fsqlite-ns-use` are fsqlite's multi-process
/// namespace admission files; they are created for *any* database path the
/// engine opens, including the `VACUUM INTO` temp target.
/// Sidecars fsqlite maintains for multi-process namespace admission.
///
/// Single source of truth — `doctor`'s database-family walk reads the same
/// constant so the two cannot drift apart.
pub(crate) const FSQLITE_NAMESPACE_SIDECAR_SUFFIXES: &[&str] =
    &["-fsqlite-ns-gate", "-fsqlite-ns-use"];

/// The parallel-WAL durability-certificate sidecars fsqlite 0.2+ maintains
/// next to the classic `-wal` file.
pub(crate) const FSQLITE_WAL_CERT_SIDECAR_SUFFIXES: &[&str] = &["-wal-cert", "-wal-cert-head"];

/// The classic SQLite sidecars.
pub(crate) const CLASSIC_SIDECAR_SUFFIXES: &[&str] = &["-wal", "-shm", "-journal"];

/// Every engine-managed sidecar suffix, classic and fsqlite-specific.
pub(crate) fn db_sidecar_suffixes() -> impl Iterator<Item = &'static &'static str> {
    CLASSIC_SIDECAR_SUFFIXES
        .iter()
        .chain(FSQLITE_NAMESPACE_SIDECAR_SUFFIXES.iter())
        .chain(FSQLITE_WAL_CERT_SIDECAR_SUFFIXES.iter())
}

/// Compact a database at `db_path` by writing a fresh copy via `VACUUM
/// INTO` to a temp file, atomically replacing the original, and returning a
/// reopened storage connection.
///
/// Preconditions: the caller must pass a `storage` handle whose connection
/// was opened against `db_path` (the helper names its temp file
/// `.<stem>.vacuum.<pid>.tmp` next to `db_path` and installs it there).
/// Passing mismatched storage and db_path would copy the storage's actual
/// DB contents over db_path.
///
/// Failure handling: on any failure (VACUUM INTO error, rename error, or
/// reopen error after a successful rename) the helper returns either the
/// best-available working handle or an error before the caller can continue:
///
/// * VACUUM INTO failed — returns the unchanged pre-compaction connection.
/// * Rename failed — returns a connection reopened against the still-intact
///   original `db_path`; the compacted temp file is removed.
/// * Reopen failed after replacing the handle — returns an error, ensuring
///   live code cannot continue on a throwaway placeholder connection.
///
/// Cosmetic compaction failures remain non-fatal when the original handle is
/// still usable. Failures after the original connection has been closed are
/// surfaced because the caller no longer has a valid persistent storage handle.
///
/// This is called only in rebuild/force-import paths where the DB is
/// known to have just been fully populated from JSONL.
pub(crate) fn compact_database_via_vacuum_into_in_place(
    storage: SqliteStorage,
    db_path: &Path,
    lock_timeout: Option<u64>,
) -> Result<SqliteStorage> {
    let write_authority =
        storage
            .attached_write_authority()
            .ok_or_else(|| BeadsError::SyncConflict {
                message:
                    "Persistent database compaction requires attached database-family authority"
                        .to_string(),
            })?;
    compact_database_via_vacuum_into_in_place_with_reopener(
        storage,
        db_path,
        lock_timeout,
        &write_authority,
        |path, timeout| {
            SqliteStorage::open_with_timeout_under_write_authority(path, timeout, &write_authority)
        },
        || Ok(()),
        || Ok(()),
        |from, to| crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io),
    )
}

fn compact_database_via_vacuum_into_in_place_under_write_authority(
    storage: SqliteStorage,
    db_path: &Path,
    lock_timeout: Option<u64>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<SqliteStorage> {
    compact_database_via_vacuum_into_in_place_with_reopener(
        storage,
        db_path,
        lock_timeout,
        write_authority,
        |path, timeout| {
            SqliteStorage::open_with_timeout_under_write_authority(path, timeout, write_authority)
        },
        || Ok(()),
        || Ok(()),
        |from, to| crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io),
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn compact_database_via_vacuum_into_in_place_with_reopener(
    storage: SqliteStorage,
    db_path: &Path,
    lock_timeout: Option<u64>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    reopen_storage: impl Fn(&Path, Option<u64>) -> Result<SqliteStorage>,
    before_candidate_install: impl FnOnce() -> Result<()>,
    after_candidate_adoption: impl FnOnce() -> Result<()>,
    sync_candidate_install: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<SqliteStorage> {
    // Drain any WAL frames the prior VACUUM/REINDEX (run by the caller)
    // left behind, so `VACUUM INTO` sees the fully-committed on-disk
    // state instead of having to reach into a WAL that fsqlite's own
    // `VACUUM INTO` may or may not consult. Keeping this inside the
    // helper means every caller gets the same guarantee regardless of
    // whether they remembered to checkpoint themselves.
    if let Err(err) = storage.checkpoint_full() {
        tracing::debug!(
            error = %err,
            db_path = %db_path.display(),
            "Pre-VACUUM-INTO WAL checkpoint failed; skipping cosmetic compaction so committed WAL frames cannot be omitted"
        );
        return Ok(storage);
    }

    // Unique temp path next to the real DB so the subsequent rename is on
    // the same filesystem (atomic) and so parallel rebuilds of different
    // DBs don't collide.
    let stem = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| "beads".to_string(), str::to_string);
    let temp_path = db_path.with_file_name(format!(
        ".{stem}.vacuum.{}.{}.tmp",
        std::process::id(),
        Utc::now().format("%Y%m%d_%H%M%S_%f")
    ));

    let temp_path_display = temp_path.display().to_string();
    // Escape single quotes the SQL way (doubling) for the literal path
    // embedded in the `VACUUM INTO` statement. The temp path is
    // constructed from the DB path + our own PID suffix, so in practice
    // it never contains a quote, but the doubling keeps us safe against
    // unusual filesystem names.
    let escaped_path = temp_path_display.replace('\'', "''");
    let vacuum_into_sql = format!("VACUUM INTO '{escaped_path}'");
    if let Err(err) = storage.execute_raw(&vacuum_into_sql) {
        tracing::warn!(
            error = %err,
            db_path = %db_path.display(),
            "`VACUUM INTO` compaction failed; keeping the in-place rebuild which may still show unused tail pages under upstream sqlite3"
        );
        // `VACUUM INTO` creates its destination with no-clobber semantics, but
        // a failed operation has not established an inode witness. Preserve
        // every resulting path instead of guessing that it is ours to delete.
        return Ok(storage);
    }
    let replacement_lock = write_authority.lock_database_replacement_candidate(&temp_path)?;

    // Close the on-disk connection before swapping its file under our own
    // feet. This helper consumes and returns the storage handle so callers
    // cannot keep using a throwaway placeholder if reopening fails.
    drop(storage);

    if let Err(error) = replacement_lock.sync_all() {
        remove_locked_compaction_candidate(write_authority, &replacement_lock, &temp_path);
        return Err(BeadsError::WithContext {
            context: format!(
                "Failed to make compacted database candidate '{}' durable before installation",
                temp_path.display()
            ),
            source: Box::new(error),
        });
    }
    if let Err(error) = before_candidate_install() {
        remove_locked_compaction_candidate(write_authority, &replacement_lock, &temp_path);
        return Err(error);
    }

    // Move the complete live family aside before installing the compacted
    // main file. Atomic no-replace renames plus the retained-inode check form
    // an expected-authority exchange: a foreign generation that wins either
    // namespace race is preserved rather than overwritten.
    let recovery_parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let compaction_stamp = format!(
        "vacuum-{}-{}",
        std::process::id(),
        Utc::now().format("%Y%m%d_%H%M%S_%f")
    );
    let retained_family =
        match move_database_family_to_recovery(db_path, recovery_parent, &compaction_stamp) {
            Ok(backup_set) => backup_set,
            Err(stage_error) => {
                remove_locked_compaction_candidate(write_authority, &replacement_lock, &temp_path);
                return Err(stage_error);
            }
        };
    let staged_database = retained_family
        .files
        .iter()
        .find_map(|(original, backup)| (original == db_path).then_some(backup));
    let staged_verification = staged_database
        .ok_or_else(|| BeadsError::SyncConflict {
            message: "Compaction could not stage the retained database generation".to_string(),
        })
        .and_then(|staged| write_authority.verify_staged_database_recovery_authority(staged))
        .and_then(|()| verify_recovery_backup_set(&retained_family));
    if let Err(authority_error) = staged_verification {
        let rollback_result = rollback_renamed_paths_no_replace(
            &retained_family.files,
            "abort compacted database installation after staged-family verification failed",
        );
        remove_locked_compaction_candidate(write_authority, &replacement_lock, &temp_path);
        if let Err(rollback_error) = rollback_result {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Compaction staged an unverified database family ({authority_error}); restoring it without replacement also failed"
                ),
                source: Box::new(rollback_error),
            });
        }
        return Err(authority_error);
    }

    if let Err(install_error) = rename_recovery_path_no_replace(&temp_path, db_path) {
        let rollback_result = rollback_renamed_paths_no_replace(
            &retained_family.files,
            "restore the retained database family after compacted installation failed",
        );
        remove_locked_compaction_candidate(write_authority, &replacement_lock, &temp_path);
        if let Err(rollback_error) = rollback_result {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to install the compacted database without replacement ({install_error}); retained the prior family in '{}' because its canonical namespace was no longer empty",
                    retained_family.recovery_dir.display()
                ),
                source: Box::new(rollback_error),
            });
        }
        return reopen_storage(db_path, lock_timeout)
            .and_then(|mut reopened| {
                write_authority.verify_database_authority()?;
                reopened.attach_write_authority(Arc::clone(write_authority));
                Ok(reopened)
            })
            .map_err(|reopen_error| BeadsError::WithContext {
                context: format!(
                    "Failed to reopen the retained database after compacted installation failed ({install_error})"
                ),
                source: Box::new(reopen_error),
            });
    }

    let post_install_result = write_authority
        .adopt_locked_database_replacement(replacement_lock)
        .and_then(|()| write_authority.verify_database_authority())
        .and_then(|()| after_candidate_adoption())
        .and_then(|()| sync_candidate_install(&temp_path, db_path))
        .and_then(|()| {
            move_private_compaction_sidecars_to_recovery(&temp_path)?;
            reopen_storage(db_path, lock_timeout)
        })
        .and_then(|mut reopened| {
            write_authority.verify_database_authority()?;
            reopened.attach_write_authority(Arc::clone(write_authority));
            Ok(reopened)
        })
        .map_err(|error| BeadsError::WithContext {
            context: format!(
                "Failed to reopen compacted database after VACUUM INTO or validate its installation at '{}'",
                db_path.display()
            ),
            source: Box::new(error),
        });

    match post_install_result {
        Ok(reopened) => Ok(reopened),
        Err(install_error) => {
            if let Err(rollback_error) =
                rollback_compacted_database_install(&retained_family, write_authority)
            {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Compacted database installation failed ({install_error}); restoring the retained pre-compaction family also failed"
                    ),
                    source: Box::new(rollback_error),
                });
            }
            Err(install_error)
        }
    }
}

fn remove_locked_compaction_candidate(
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    replacement_lock: &fs::File,
    temp_path: &Path,
) {
    if let Err(error) =
        write_authority.verify_locked_database_replacement_candidate(temp_path, replacement_lock)
    {
        tracing::warn!(
            error = %error,
            temp_path = %temp_path.display(),
            "Refusing to remove a compaction candidate path that no longer names the locked candidate"
        );
        return;
    }
    if let Err(error) = fs::remove_file(temp_path) {
        tracing::warn!(
            error = %error,
            temp_path = %temp_path.display(),
            "Failed to remove an unused locked compaction candidate"
        );
    } else if let Err(error) = crate::util::sync_parent_directory(temp_path) {
        tracing::warn!(
            error = %error,
            temp_path = %temp_path.display(),
            "Unused locked compaction candidate was removed, but the namespace change was not made durable"
        );
    }
    if let Err(error) = move_private_compaction_sidecars_to_recovery(temp_path) {
        tracing::warn!(
            error = %error,
            temp_path = %temp_path.display(),
            "Failed to retain private compaction sidecars in recovery"
        );
    }
}

fn move_private_compaction_sidecars_to_recovery(temp_path: &Path) -> Result<()> {
    let recovery_parent = temp_path.parent().unwrap_or_else(|| Path::new("."));
    let mut sidecar_backup =
        prepare_missing_database_cleanup_for_recovery(temp_path, recovery_parent)?;
    move_orphaned_database_sidecars_to_recovery(&mut sidecar_backup)?;
    verify_recovery_backup_set(&sidecar_backup)
}

fn rollback_compacted_database_install(
    retained_family: &RecoveryBackupSet,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
) -> Result<()> {
    restore_database_family_after_failed_rebuild_before_live_staging(
        retained_family,
        || {
            if write_authority.database_target_authority_state()?
                == crate::sync::DatabaseTargetAuthorityState::Held
            {
                return Ok(());
            }
            Err(BeadsError::SyncConflict {
                message: "Refusing compaction rollback because the installed database is no longer the retained candidate generation"
                    .to_string(),
            })
        },
        |staged_paths| {
            verify_staged_database_recovery_authority(
                retained_family,
                staged_paths,
                write_authority,
                crate::sync::DatabaseTargetAuthorityState::Held,
            )
        },
    )?;
    write_authority.restore_nested_retained_database_inode_after_authorized_replace()?;
    write_authority.verify_database_authority()
}

fn rebuild_database_family_with_backup<T, F>(
    db_path: &Path,
    beads_dir: &Path,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    successful_disposition: SuccessfulRecoveryDisposition,
    rebuild: F,
) -> Result<(T, RecoveryBackupSet)>
where
    F: FnOnce(FreshDatabaseReplacementWitness) -> Result<T>,
{
    rebuild_database_family_with_backup_before_install(
        db_path,
        beads_dir,
        write_authority,
        successful_disposition,
        || {},
        rebuild,
    )
}

fn rebuild_database_family_with_backup_before_install<T, B, F>(
    db_path: &Path,
    beads_dir: &Path,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    successful_disposition: SuccessfulRecoveryDisposition,
    before_install: B,
    rebuild: F,
) -> Result<(T, RecoveryBackupSet)>
where
    B: FnOnce(),
    F: FnOnce(FreshDatabaseReplacementWitness) -> Result<T>,
{
    let mut backup_set =
        prepare_database_family_backup_for_recovery(db_path, beads_dir, write_authority)?;
    before_install();
    let staged_backup_verification = verify_recovery_backup_set(&backup_set).and_then(|()| {
        if !backup_set.had_original_database {
            return Ok(());
        }
        let staged_database = backup_set
            .files
            .iter()
            .find_map(|(original, backup)| (original == db_path).then_some(backup))
            .ok_or_else(|| BeadsError::SyncConflict {
                message: "Recovery backup manifest lost its staged main database".to_string(),
            })?;
        write_authority.verify_staged_database_recovery_authority(staged_database)
    });
    if let Err(verification_error) = staged_backup_verification {
        if let Err(rollback_error) = rollback_renamed_paths_no_replace(
            &backup_set.files,
            "abort database recovery before fresh installation after staged-backup verification failed",
        ) {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Staged recovery backup verification failed ({verification_error}); restoring the staged family without replacement also failed"
                ),
                source: Box::new(rollback_error),
            });
        }
        return Err(verification_error);
    }
    let fresh_witness = match write_authority.install_empty_database_replacement_and_bind() {
        Ok(witness) => witness,
        Err(install_err) => {
            return Err(finish_failed_database_recovery(
                &backup_set,
                write_authority,
                install_err,
                RecoveryFailurePhase::Install,
            ));
        }
    };
    if !backup_set.had_original_database
        && let Err(sidecar_err) = write_authority
            .verify_fresh_database_replacement_witness(&fresh_witness)
            .and_then(|()| move_orphaned_database_sidecars_to_recovery(&mut backup_set))
    {
        return Err(finish_failed_database_recovery(
            &backup_set,
            write_authority,
            sidecar_err,
            RecoveryFailurePhase::Install,
        ));
    }

    match rebuild(fresh_witness) {
        Ok(value)
            if successful_disposition
                == SuccessfulRecoveryDisposition::RetainBackupUntilCommandSuccess =>
        {
            Ok((value, backup_set))
        }
        Ok(value) => match write_authority.finalize_database_replacement() {
            Ok(()) => Ok((value, backup_set)),
            Err(finalize_err) => {
                drop(value);
                Err(finish_failed_database_recovery(
                    &backup_set,
                    write_authority,
                    finalize_err,
                    RecoveryFailurePhase::Rebuild,
                ))
            }
        },
        Err(rebuild_err) => Err(finish_failed_database_recovery(
            &backup_set,
            write_authority,
            rebuild_err,
            RecoveryFailurePhase::Rebuild,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryFailurePhase {
    Install,
    Rebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryRollbackDisposition {
    Restored,
    PreservedLiveTarget,
}

fn finish_failed_database_recovery(
    backup_set: &RecoveryBackupSet,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    recovery_err: BeadsError,
    phase: RecoveryFailurePhase,
) -> BeadsError {
    match rollback_database_family_after_recovery_failure(backup_set, write_authority, phase) {
        Ok(RecoveryRollbackDisposition::Restored) => recovery_err,
        Ok(RecoveryRollbackDisposition::PreservedLiveTarget) => BeadsError::WithContext {
            context: format!(
                "Database recovery stopped after another canonical database generation appeared; left it untouched and retained recovery artifacts in '{}'",
                backup_set.recovery_dir.display()
            ),
            source: Box::new(recovery_err),
        },
        Err(restore_err) => recovery_restore_failure(backup_set, &recovery_err, restore_err),
    }
}

fn rollback_database_family_after_recovery_failure(
    backup_set: &RecoveryBackupSet,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    phase: RecoveryFailurePhase,
) -> Result<RecoveryRollbackDisposition> {
    use crate::sync::DatabaseTargetAuthorityState;

    let target_state = write_authority.database_target_authority_state()?;
    let live_sidecar_exists = database_family_has_live_sidecars(&backup_set.db_path)?;
    let preserve_live_target = target_state == DatabaseTargetAuthorityState::Foreign
        || (target_state == DatabaseTargetAuthorityState::Missing && live_sidecar_exists)
        || (phase == RecoveryFailurePhase::Install
            && target_state == DatabaseTargetAuthorityState::Held
            && live_sidecar_exists);
    if preserve_live_target {
        warn!(
            db_path = %backup_set.db_path.display(),
            recovery_dir = %backup_set.recovery_dir.display(),
            ?target_state,
            "Preserving a live database generation that recovery does not exclusively own"
        );
        return Ok(RecoveryRollbackDisposition::PreservedLiveTarget);
    }

    restore_database_family_after_failed_rebuild_before_live_staging(
        backup_set,
        || {
            let late_target_state = write_authority.database_target_authority_state()?;
            let late_sidecar_exists = database_family_has_live_sidecars(&backup_set.db_path)?;
            let late_target_must_be_preserved = late_target_state
                == DatabaseTargetAuthorityState::Foreign
                || (late_target_state == DatabaseTargetAuthorityState::Missing
                    && late_sidecar_exists)
                || (phase == RecoveryFailurePhase::Install
                    && late_target_state == DatabaseTargetAuthorityState::Held
                    && late_sidecar_exists);
            if late_target_state != target_state || late_target_must_be_preserved {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Refusing recovery rollback because the live database family changed while its backups were being claimed (expected {target_state:?}, found {late_target_state:?}); it was left untouched"
                    ),
                });
            }
            Ok(())
        },
        |staged_paths| {
            verify_staged_database_recovery_authority(
                backup_set,
                staged_paths,
                write_authority,
                target_state,
            )
        },
    )?;
    if backup_set.had_original_database {
        write_authority.restore_retained_database_inode_after_authorized_replace()?;
    } else {
        write_authority.clear_database_inode_after_authorized_remove()?;
    }
    write_authority.verify_database_authority()?;
    Ok(RecoveryRollbackDisposition::Restored)
}

fn database_family_has_live_sidecars(db_path: &Path) -> Result<bool> {
    database_family_paths(db_path)
        .into_iter()
        .skip(1)
        .try_fold(false, |found, path| match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(found),
            Err(error) => Err(BeadsError::WithContext {
                context: format!(
                    "Failed to inspect live database-family sidecar '{}' during recovery rollback",
                    path.display()
                ),
                source: Box::new(error),
            }),
        })
}

fn verify_staged_database_recovery_authority(
    backup_set: &RecoveryBackupSet,
    staged_paths: &[RecoveryBackupPath],
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
    expected_target_state: crate::sync::DatabaseTargetAuthorityState,
) -> Result<()> {
    use crate::sync::DatabaseTargetAuthorityState;

    let staged_database = staged_paths
        .iter()
        .find_map(|(original, staged)| (original == &backup_set.db_path).then_some(staged));
    match expected_target_state {
        DatabaseTargetAuthorityState::Held => {
            let staged_database = staged_database.ok_or_else(|| BeadsError::SyncConflict {
                message: "The retained database generation disappeared before recovery rollback could stage it"
                    .to_string(),
            })?;
            write_authority.verify_staged_database_recovery_authority(staged_database)
        }
        DatabaseTargetAuthorityState::Missing if staged_database.is_none() => Ok(()),
        DatabaseTargetAuthorityState::Missing => Err(BeadsError::SyncConflict {
            message: "A database generation appeared after the final missing-target recovery check; refusing to install the original backup"
                .to_string(),
        }),
        DatabaseTargetAuthorityState::Foreign => Err(BeadsError::SyncConflict {
            message: "A foreign database generation reached recovery staging; refusing to install the original backup"
                .to_string(),
        }),
    }
}

fn prepare_database_family_backup_for_recovery(
    db_path: &Path,
    beads_dir: &Path,
    write_authority: &crate::sync::DatabaseFamilyWriteLock,
) -> Result<RecoveryBackupSet> {
    if write_authority.bind_database_inode_for_mutation()? {
        prepare_missing_database_cleanup_for_recovery(db_path, beads_dir)
    } else {
        backup_database_family_for_recovery(db_path, beads_dir)
    }
}

fn backup_database_family_for_recovery(
    db_path: &Path,
    beads_dir: &Path,
) -> Result<RecoveryBackupSet> {
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
    move_database_family_to_recovery(db_path, beads_dir, &stamp)
}

fn prepare_missing_database_cleanup_for_recovery(
    db_path: &Path,
    beads_dir: &Path,
) -> Result<RecoveryBackupSet> {
    reject_symlinked_database_path_for_recovery(db_path)?;
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
    let recovery_dir = recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;
    Ok(RecoveryBackupSet {
        db_path: db_path.to_path_buf(),
        recovery_dir,
        stamp,
        had_original_database: false,
        files: Vec::new(),
        verified_files: Vec::new(),
    })
}

fn move_orphaned_database_sidecars_to_recovery(backup_set: &mut RecoveryBackupSet) -> Result<()> {
    debug_assert!(!backup_set.had_original_database);
    let (files, verified_files) = rename_existing_paths_with_backup_verification(
        database_family_paths(&backup_set.db_path)
            .into_iter()
            .skip(1)
            .map(|original| {
                let backup = backup_set.recovery_dir.join(recovery_backup_filename(
                    &original,
                    &backup_set.stamp,
                    "bak",
                ));
                (original, backup)
            }),
        "move orphaned database sidecars into recovery",
        MissingRenameSourcePolicy::Skip,
    )?;
    backup_set.files.extend(files);
    backup_set.verified_files.extend(verified_files);
    Ok(())
}

fn move_database_family_to_recovery(
    db_path: &Path,
    beads_dir: &Path,
    stamp: &str,
) -> Result<RecoveryBackupSet> {
    reject_symlinked_database_path_for_recovery(db_path)?;
    let recovery_dir = recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;
    let (files, verified_files) = rename_existing_paths_with_backup_verification(
        database_family_paths(db_path).into_iter().map(|original| {
            let backup = recovery_dir.join(recovery_backup_filename(&original, stamp, "bak"));
            (original, backup)
        }),
        "move the database family into recovery",
        MissingRenameSourcePolicy::Skip,
    )?;

    let had_original_database = files.iter().any(|(original, _)| original == db_path);
    Ok(RecoveryBackupSet {
        db_path: db_path.to_path_buf(),
        recovery_dir,
        stamp: stamp.to_string(),
        had_original_database,
        files,
        verified_files,
    })
}

#[derive(Debug)]
struct ClaimedRecoveryBackup {
    original: PathBuf,
    backup: PathBuf,
    claimed: PathBuf,
}

fn recovery_restore_claim_path(
    backup_set: &RecoveryBackupSet,
    backup: &Path,
    nonce: &str,
    ordinal: usize,
) -> PathBuf {
    let filename = backup
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("beads.db.bak");
    backup_set
        .recovery_dir
        .join(format!(".{filename}.restore-staging.{nonce}.{ordinal}.bak"))
}

fn claimed_recovery_backup_paths(
    claimed_backups: &[ClaimedRecoveryBackup],
) -> Vec<RecoveryBackupPath> {
    claimed_backups
        .iter()
        .map(|claimed| (claimed.backup.clone(), claimed.claimed.clone()))
        .collect()
}

fn rollback_claimed_recovery_backups(
    claimed_backups: &[ClaimedRecoveryBackup],
    operation: &str,
) -> Result<()> {
    rollback_renamed_paths_no_replace(&claimed_recovery_backup_paths(claimed_backups), operation)
}

fn claim_recovery_backup_set_for_restore(
    backup_set: &RecoveryBackupSet,
) -> Result<Vec<ClaimedRecoveryBackup>> {
    verify_recovery_backup_manifest(backup_set)?;
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        Utc::now().format("%Y%m%d_%H%M%S_%f")
    );
    let claim_paths = backup_set
        .files
        .iter()
        .enumerate()
        .map(|(ordinal, (_, backup))| {
            (
                backup.clone(),
                recovery_restore_claim_path(backup_set, backup, &nonce, ordinal),
            )
        });
    let claimed_paths = rename_existing_paths_no_replace(
        claim_paths,
        "claim verified recovery backups for restore",
        MissingRenameSourcePolicy::Error,
    )?;
    let claimed_backups = backup_set
        .files
        .iter()
        .zip(claimed_paths)
        .map(|((original, backup), (claimed_backup, claimed))| {
            debug_assert_eq!(backup, &claimed_backup);
            ClaimedRecoveryBackup {
                original: original.clone(),
                backup: backup.clone(),
                claimed,
            }
        })
        .collect::<Vec<_>>();

    for (claimed, verification) in claimed_backups.iter().zip(&backup_set.verified_files) {
        if let Err(verify_error) = verify_recovery_backup_artifact(
            &claimed.claimed,
            &expected_recovery_artifact_fingerprint(verification),
        ) {
            if let Err(rollback_error) = rollback_claimed_recovery_backups(
                &claimed_backups,
                "claim verified recovery backups for restore",
            ) {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Claimed recovery backup '{}' failed verification ({verify_error}); returning claimed backups to their public recovery names also failed",
                        claimed.claimed.display()
                    ),
                    source: Box::new(rollback_error),
                });
            }
            return Err(BeadsError::WithContext {
                context: format!(
                    "Refusing database-family restore because claimed recovery backup '{}' no longer matches its recorded fingerprint; the live database was left untouched",
                    claimed.claimed.display()
                ),
                source: Box::new(verify_error),
            });
        }
    }

    Ok(claimed_backups)
}

#[cfg(test)]
fn restore_database_family_after_failed_rebuild(backup_set: &RecoveryBackupSet) -> Result<()> {
    restore_database_family_after_failed_rebuild_before_live_staging(
        backup_set,
        || Ok(()),
        |_| Ok(()),
    )
}

fn restore_database_family_after_failed_rebuild_before_live_staging<B, A>(
    backup_set: &RecoveryBackupSet,
    before_live_staging: B,
    after_live_staging: A,
) -> Result<()>
where
    B: FnOnce() -> Result<()>,
    A: FnOnce(&[RecoveryBackupPath]) -> Result<()>,
{
    let live_family_witness = capture_live_database_family_witness(&backup_set.db_path)?;
    restore_database_family_after_failed_rebuild_with_hooks(
        backup_set,
        before_live_staging,
        |staged_paths| {
            verify_staged_database_family_witness(&live_family_witness, staged_paths)?;
            after_live_staging(staged_paths)
        },
    )
}

fn database_family_has_live_paths(db_path: &Path) -> Result<bool> {
    database_family_paths(db_path)
        .into_iter()
        .try_fold(false, |found, path| match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(found),
            Err(error) => Err(BeadsError::WithContext {
                context: format!(
                    "Failed to inspect live database-family path '{}' during recovery rollback",
                    path.display()
                ),
                source: Box::new(error),
            }),
        })
}

#[allow(clippy::too_many_lines)]
fn restore_database_family_after_failed_rebuild_with_hooks<B, R>(
    backup_set: &RecoveryBackupSet,
    before_live_staging: B,
    before_claimed_restore: R,
) -> Result<()>
where
    B: FnOnce() -> Result<()>,
    R: FnOnce(&[RecoveryBackupPath]) -> Result<()>,
{
    let claimed_backups = claim_recovery_backup_set_for_restore(backup_set)?;
    if let Err(authority_error) = before_live_staging() {
        if let Err(rollback_error) = rollback_claimed_recovery_backups(
            &claimed_backups,
            "abort recovery restore before staging the live database family",
        ) {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Recovery restore authority check failed ({authority_error}); returning claimed backups to their public recovery names also failed"
                ),
                source: Box::new(rollback_error),
            });
        }
        return Err(authority_error);
    }

    let rebuilt_backups = match rename_existing_paths_no_replace(
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
    ) {
        Ok(rebuilt_backups) => rebuilt_backups,
        Err(stage_error) => {
            if let Err(rollback_error) = rollback_claimed_recovery_backups(
                &claimed_backups,
                "abort recovery restore after rebuilt-family staging failed",
            ) {
                return Err(BeadsError::WithContext {
                    context: format!(
                        "Failed to stage rebuilt database files ({stage_error}); returning claimed backups to their public recovery names also failed"
                    ),
                    source: Box::new(rollback_error),
                });
            }
            return Err(stage_error);
        }
    };

    let restore_result = before_claimed_restore(&rebuilt_backups).and_then(|()| {
        rename_existing_paths_no_replace(
            claimed_backups
                .iter()
                .map(|claimed| (claimed.claimed.clone(), claimed.original.clone())),
            "restore the claimed original database family after failed recovery",
            MissingRenameSourcePolicy::Error,
        )
        .map(|_| ())
    });

    if let Err(restore_error) = restore_result {
        let live_family_inspection = database_family_has_live_paths(&backup_set.db_path);
        let live_family_present = live_family_inspection
            .as_ref()
            .map_or(true, |present| *present);
        let claimed_rollback = rollback_claimed_recovery_backups(
            &claimed_backups,
            "abort recovery restore after claimed-backup restore failed",
        );
        let rebuilt_rollback = if live_family_present {
            Ok(())
        } else {
            rollback_renamed_paths_no_replace(
                &rebuilt_backups,
                "restore the rebuilt database family after original-family restore failed",
            )
        };

        if let Err(rollback_error) = claimed_rollback {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to restore the claimed original database family ({restore_error}); returning claimed backups to their public recovery names also failed"
                ),
                source: Box::new(rollback_error),
            });
        }
        if let Err(rollback_error) = rebuilt_rollback {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to restore the claimed original database family ({restore_error}); restoring the staged rebuilt database family without replacement also failed"
                ),
                source: Box::new(rollback_error),
            });
        }
        if let Err(inspection_error) = live_family_inspection {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to restore the claimed original database family ({restore_error}); refused to replace the staged rebuilt family because the live namespace could not be inspected"
                ),
                source: Box::new(inspection_error),
            });
        }
        if live_family_present {
            return Err(BeadsError::WithContext {
                context:
                    "Refused to overwrite a database-family path that appeared during recovery restore; preserved that live generation and retained the failed rebuilt generation in recovery"
                        .to_string(),
                source: Box::new(restore_error),
            });
        }
        return Err(restore_error);
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
    std::iter::once(db_path.to_path_buf())
        .chain(db_sidecar_suffixes().map(|suffix| {
            let mut sidecar = db_path.as_os_str().to_os_string();
            sidecar.push(suffix);
            PathBuf::from(sidecar)
        }))
        .collect()
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
    Ok(quarantine_database_artifacts_with_original_paths(
        db_path,
        beads_dir,
        artifact_paths,
        suffix,
    )?
    .into_iter()
    .map(|(_, backup)| backup)
    .collect())
}

fn quarantine_database_artifacts_with_original_paths<I>(
    db_path: &Path,
    beads_dir: &Path,
    artifact_paths: I,
    suffix: &str,
) -> Result<Vec<RecoveryBackupPath>>
where
    I: IntoIterator<Item = PathBuf>,
{
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();
    let recovery_dir = recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;

    let (renamed_paths, verified_files) = rename_existing_paths_with_backup_verification(
        artifact_paths.into_iter().map(|original| {
            let backup = recovery_dir.join(recovery_backup_filename(&original, &stamp, suffix));
            (original, backup)
        }),
        "quarantine database artifacts",
        MissingRenameSourcePolicy::Skip,
    )?;

    tracing::warn!(
        db_path = %db_path.display(),
        recovery_dir = %recovery_dir.display(),
        verified_backup_count = verified_files.len(),
        verified_backups = ?verified_files,
        "Verified quarantined database artifact backups"
    );

    Ok(renamed_paths)
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

    let write_authority = Arc::new(blocking_database_family_write_lock_with_timeout(
        beads_dir,
        &startup.paths.db_path,
        resolved_lock_timeout,
    )?);
    write_authority.bind_database_inode_for_mutation()?;
    let mut open_result = open_sqlite_storage_with_recovery(
        beads_dir,
        &startup.paths,
        resolved_lock_timeout,
        &merged_layer,
        false,
        Some(&write_authority),
    )?;
    write_authority.verify_database_authority()?;
    open_result.storage.attach_write_authority(write_authority);
    let workflow = crate::close_policy::load_for_beads_dir(beads_dir)?.workflow;
    open_result.storage.set_workflow_policy(workflow);
    Ok((open_result.storage, startup.paths))
}

/// Storage handle with no-db awareness.
#[derive(Debug)]
enum RetainedJsonlSource {
    Uncaptured,
    Missing,
    Present(Arc<JsonlSourceSnapshot>),
}

#[derive(Clone, Copy)]
pub(crate) enum RetainedJsonlSourceRef<'a> {
    Uncaptured,
    Missing,
    Present(&'a JsonlSourceSnapshot),
}

#[derive(Clone, Copy)]
enum JsonlConfigSource<'a> {
    Uncaptured,
    Missing,
    Present(&'a JsonlSourceSnapshot),
}

#[derive(Debug)]
pub struct OpenStorageResult {
    pub storage: SqliteStorage,
    pub paths: ConfigPaths,
    pub no_db: bool,
    write_authority: Option<Arc<crate::sync::DatabaseFamilyWriteLock>>,
    jsonl_write_authority: Option<Arc<crate::sync::JsonlFamilyWriteLock>>,
    /// True when the SQLite DB file was just rebuilt from JSONL during this
    /// `open_storage_with_cli` call (either because the file didn't exist, or
    /// because a recoverable anomaly was detected after opening). Callers that
    /// would otherwise re-run a full rebuild (e.g.
    /// `br sync --import-only --rebuild`) can skip the redundant work — the DB
    /// is already a fresh import.
    pub auto_rebuilt: bool,
    allow_external_jsonl: bool,
    startup_layers: Vec<ConfigLayer>,
    bootstrap_layer: ConfigLayer,
    resolved_lock_timeout: Option<u64>,
    loaded_jsonl_state: JsonlSourceStateWitness,
    loaded_jsonl_source: RetainedJsonlSource,
    pending_recovery_backup: Option<RecoveryBackupSet>,
    /// Set only when a storage-replacement transition has released the live
    /// connection and cannot restore a verified persistent handle. The
    /// state-carrying temporary storage remains available for orderly error
    /// propagation, but JSONL recovery must not mistake it for the database
    /// generation that failed.
    database_transition_failed_closed: bool,
}

impl OpenStorageResult {
    /// Clone the database-family authority retained by a writable startup
    /// fallback, if any.
    ///
    /// Read-only fast-open can discover that schema healing is required and
    /// acquire authority inside the config layer. Startup orchestration must
    /// reuse that exact capability for a subsequent JSONL freshness reprobe;
    /// opening the same lock through another descriptor would self-deadlock.
    #[doc(hidden)]
    #[must_use]
    pub fn retained_database_write_authority(
        &self,
    ) -> Option<Arc<crate::sync::DatabaseFamilyWriteLock>> {
        self.write_authority.as_ref().map(Arc::clone)
    }

    /// Reconstruct the immutable startup snapshot that opened this context.
    ///
    /// Protected fast-open reopens use this snapshot so a concurrent metadata
    /// or config-file rewrite cannot mix storage from one routing generation
    /// with JSONL from another.
    #[doc(hidden)]
    #[must_use]
    pub fn retained_startup_config(&self) -> StartupConfig {
        StartupConfig {
            paths: self.paths.clone(),
            layers: self.startup_layers.clone(),
            merged_config: ConfigLayer::merge_layers(&self.startup_layers),
        }
    }

    pub(crate) fn import_context(
        &mut self,
    ) -> (
        &mut SqliteStorage,
        RetainedJsonlSourceRef<'_>,
        Option<&crate::sync::JsonlFamilyWriteLock>,
    ) {
        let source = match &self.loaded_jsonl_source {
            RetainedJsonlSource::Uncaptured => RetainedJsonlSourceRef::Uncaptured,
            RetainedJsonlSource::Missing => RetainedJsonlSourceRef::Missing,
            RetainedJsonlSource::Present(source) => {
                RetainedJsonlSourceRef::Present(source.as_ref())
            }
        };
        (
            &mut self.storage,
            source,
            self.jsonl_write_authority.as_deref(),
        )
    }

    pub(crate) fn jsonl_write_context(
        &mut self,
    ) -> (
        &mut SqliteStorage,
        RetainedJsonlSourceRef<'_>,
        &JsonlSourceStateWitness,
        Option<&crate::sync::JsonlFamilyWriteLock>,
    ) {
        let source = match &self.loaded_jsonl_source {
            RetainedJsonlSource::Uncaptured => RetainedJsonlSourceRef::Uncaptured,
            RetainedJsonlSource::Missing => RetainedJsonlSourceRef::Missing,
            RetainedJsonlSource::Present(source) => {
                RetainedJsonlSourceRef::Present(source.as_ref())
            }
        };
        (
            &mut self.storage,
            source,
            &self.loaded_jsonl_state,
            self.jsonl_write_authority.as_deref(),
        )
    }

    /// Load the full merged config while reusing startup layers already read
    /// during storage resolution.
    ///
    /// # Errors
    ///
    /// Returns an error if JSONL prefix inference or DB-backed config loading fails.
    pub fn load_config(&self, cli: &CliOverrides) -> Result<ConfigLayer> {
        let jsonl_source = match &self.loaded_jsonl_source {
            RetainedJsonlSource::Uncaptured => JsonlConfigSource::Uncaptured,
            RetainedJsonlSource::Missing => JsonlConfigSource::Missing,
            RetainedJsonlSource::Present(source) => JsonlConfigSource::Present(source.as_ref()),
        };
        load_config_from_startup_layers(
            &self.startup_layers,
            &self.paths.beads_dir,
            &self.paths.jsonl_path,
            self.allow_external_jsonl,
            jsonl_source,
            Some(&self.storage),
            cli,
        )
    }

    /// Classify the workspace using the canonical health module.
    ///
    /// Combines file-level checks with storage-level anomaly detection
    /// to produce a single [`WorkspaceClassification`] regardless of
    /// which command triggers the evaluation.
    ///
    /// # Errors
    ///
    /// Returns an error if storage probing fails.
    pub fn classify(&self) -> Result<crate::health::WorkspaceClassification> {
        use crate::health::{WorkspaceClassification, classify_file_state};

        let mut anomalies = classify_file_state(&self.paths.db_path, &self.paths.jsonl_path);

        if !self.no_db {
            let storage_anomalies = self.storage.detect_anomalies()?;
            anomalies.extend(storage_anomalies);
        }

        Ok(WorkspaceClassification::from_anomalies(anomalies))
    }

    #[must_use]
    pub(crate) fn should_attempt_jsonl_recovery(&self, err: &BeadsError) -> bool {
        !self.database_transition_failed_closed
            && !self.no_db
            && should_attempt_jsonl_recovery(err, &self.paths.db_path, &self.paths.jsonl_path)
    }

    /// True when a storage-replacement transition could not restore a
    /// verified persistent database handle. Callers must surface the
    /// transition error instead of attempting recovery through the
    /// state-carrying in-memory sentinel.
    #[must_use]
    pub(crate) const fn database_transition_is_fail_closed(&self) -> bool {
        self.database_transition_failed_closed
    }

    #[cfg(test)]
    pub(crate) fn mark_database_transition_failed_closed_for_test(&mut self) {
        self.database_transition_failed_closed = true;
    }

    /// Rebuild the current SQLite database from the resolved JSONL export.
    ///
    /// On success, `auto_rebuilt` is set to `true` so downstream code can
    /// detect that the storage is now a fresh import of the JSONL and skip
    /// redundant rebuilds (for example, `br sync --import-only --rebuild`
    /// short-circuits when it sees this flag).
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
        if self.database_transition_failed_closed {
            return Err(BeadsError::SyncConflict {
                message: "Refusing JSONL recovery because a prior database-handle transition could not restore a verified persistent storage connection"
                    .to_string(),
            });
        }
        let write_authority =
            self.write_authority
                .clone()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "Database recovery requires an owned database-family authority"
                        .to_string(),
                })?;
        write_authority.verify_database_authority()?;

        // Preserve any attribution staged on the storage being replaced so the
        // post-recovery retry can still stamp it (#312 hardening, F1). The
        // failed first write did NOT commit, so `mutate()` left the staged value
        // intact — but recovery swaps in a brand-new `SqliteStorage`, which would
        // otherwise start with an empty pending slot and drop the attribution.
        let preserved_attribution = self.storage.take_pending_event_attribution();
        let preserved_workflow_policy = self.storage.workflow_policy();
        let import_config = import_config_for_resolved_jsonl(
            &self.paths.beads_dir,
            &self.paths.db_path,
            &self.paths.jsonl_path,
            self.allow_external_jsonl,
        );
        validate_sync_path_with_external(
            &self.paths.jsonl_path,
            &self.paths.beads_dir,
            import_config.allow_external_jsonl,
        )?;
        let jsonl_authority = Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
            &self.paths.jsonl_path,
            self.resolved_lock_timeout,
        )?);
        jsonl_authority.verify_jsonl_authority()?;
        let source = Arc::new(crate::sync::capture_jsonl_source_snapshot(
            &self.paths.jsonl_path,
        )?);
        let (preserved_tombstones, preserved_dirty_issues) =
            preserved_unflushed_state(&self.storage, &source);

        // Close the old connection before rebuilding at the same path.
        // fsqlite tracks pages by file path, so keeping the old connection
        // open while creating a new database at the same path causes
        // BusySnapshot conflicts.
        self.storage = SqliteStorage::open_memory()?;

        let (storage, _, backup_set) =
            repair_database_from_jsonl_snapshot_with_import_config_deferred(
                &self.paths.beads_dir,
                &self.paths.db_path,
                self.resolved_lock_timeout,
                &self.bootstrap_layer,
                &import_config,
                &source,
                &jsonl_authority,
                &write_authority,
            )?;
        self.storage = storage;
        self.pending_recovery_backup = Some(backup_set);
        restore_tombstones_after_rebuild(&mut self.storage, &preserved_tombstones)?;
        restore_dirty_issues_after_rebuild(&mut self.storage, &preserved_dirty_issues)?;
        write_authority.verify_database_authority()?;
        self.storage
            .attach_write_authority(Arc::clone(&write_authority));
        self.storage.set_workflow_policy(preserved_workflow_policy);
        if let Some(attribution) = preserved_attribution {
            self.storage.set_pending_event_attribution(attribution);
        }
        self.loaded_jsonl_state = source.state_witness();
        self.loaded_jsonl_source = RetainedJsonlSource::Present(source);
        self.jsonl_write_authority = Some(jsonl_authority);
        self.auto_rebuilt = true;
        Ok(())
    }

    /// GitHub #441: quarantine the parallel-WAL durability-certificate
    /// sidecars (`-wal-cert`, `-wal-cert-head`) into `.br_recovery` as
    /// verified backups and reopen the same database file in place.
    ///
    /// Used when a write fails with
    /// [`is_stale_wal_certificate_version_error`]: the certificate is derived
    /// state left behind by a different fsqlite engine generation, and the
    /// engine regenerates it on the next successful write. The `-wal` file
    /// itself is never touched — it is real data, not derived state — and
    /// this path deliberately avoids the JSONL rebuild machinery so recovery
    /// cannot wedge on a stale JSONL merge anchor.
    ///
    /// Returns the recovery-directory backup paths of the quarantined
    /// sidecars (empty when neither sidecar existed on disk).
    ///
    /// # Errors
    ///
    /// Returns an error in `--no-db` mode, without an owned database-family
    /// authority, when the quarantine rename/verification fails, or when the
    /// database cannot be reopened afterwards.
    pub(crate) fn quarantine_stale_wal_certificate_sidecars_and_reopen(
        &mut self,
    ) -> Result<Vec<PathBuf>> {
        if self.no_db {
            return Err(BeadsError::Config(
                "cannot quarantine WAL-certificate sidecars while --no-db mode is active"
                    .to_string(),
            ));
        }
        let write_authority =
            self.write_authority
                .clone()
                .ok_or_else(|| {
                    BeadsError::SyncConflict {
                message:
                    "WAL-certificate sidecar quarantine requires an owned database-family authority"
                        .to_string(),
            }
                })?;
        write_authority.verify_database_authority()?;

        let quarantine_db_path = self.paths.db_path.clone();
        let reopen_db_path = self.paths.db_path.clone();
        let quarantine_beads_dir = self.paths.beads_dir.clone();
        let resolved_lock_timeout = self.resolved_lock_timeout;
        let reopen_authority = Arc::clone(&write_authority);
        let verify_authority = Arc::clone(&write_authority);
        self.quarantine_stale_wal_certificate_sidecars_and_reopen_with(
            move || {
                let db_path_str = quarantine_db_path.to_string_lossy();
                quarantine_database_artifacts_with_original_paths(
                    &quarantine_db_path,
                    &quarantine_beads_dir,
                    FSQLITE_WAL_CERT_SIDECAR_SUFFIXES
                        .iter()
                        .map(|suffix| PathBuf::from(format!("{db_path_str}{suffix}"))),
                    "stale-wal-cert",
                )
            },
            move || {
                SqliteStorage::open_with_timeout_under_write_authority(
                    &reopen_db_path,
                    resolved_lock_timeout,
                    &reopen_authority,
                )
            },
            move || verify_authority.verify_database_authority(),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn quarantine_stale_wal_certificate_sidecars_and_reopen_with<Q, R, V>(
        &mut self,
        quarantine_sidecars: Q,
        mut reopen_storage: R,
        mut verify_database_authority: V,
    ) -> Result<Vec<PathBuf>>
    where
        Q: FnOnce() -> Result<Vec<RecoveryBackupPath>>,
        R: FnMut() -> Result<SqliteStorage>,
        V: FnMut() -> Result<()>,
    {
        if self.database_transition_failed_closed {
            return Err(BeadsError::SyncConflict {
                message: "Refusing WAL-certificate quarantine because a prior database-handle transition did not restore verified persistent storage"
                    .to_string(),
            });
        }

        // Preserve state staged on the storage being replaced, mirroring
        // `recover_database_from_jsonl` (#312 hardening, F1): the failed
        // write did not commit, so the retry must still see it.
        // Prepare and fully populate the fail-closed sentinel before releasing
        // the live connection. If creating it fails, the live handle and its
        // staged attribution remain untouched.
        let mut fail_closed_storage = SqliteStorage::open_memory()?;
        let preserved_workflow_policy = self.storage.workflow_policy();
        let preserved_attribution = self.storage.take_pending_event_attribution();
        fail_closed_storage.set_workflow_policy(preserved_workflow_policy.clone());
        if let Some(attribution) = preserved_attribution.as_ref() {
            fail_closed_storage.set_pending_event_attribution(attribution.clone());
        }

        // Close the live connection before renaming sidecars out of the
        // family: the engine caches descriptors for the certificate sidecar,
        // and a retry through the old handle could keep reading the stale
        // bytes through the renamed inode.
        let live_storage = std::mem::replace(&mut self.storage, fail_closed_storage);
        self.database_transition_failed_closed = true;
        drop(live_storage);

        let quarantined = match quarantine_sidecars() {
            Ok(paths) => paths,
            Err(quarantine_error) => {
                if let Err(authority_error) = verify_database_authority() {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "WAL-certificate quarantine failed ({quarantine_error}); refusing to reopen or JSONL-recover because database authority also changed"
                        ),
                        source: Box::new(authority_error),
                    });
                }
                let mut restored_storage = reopen_storage().map_err(|reopen_error| {
                    BeadsError::WithContext {
                        context: format!(
                            "WAL-certificate quarantine failed ({quarantine_error}); its rollback did not yield a reopenable persistent storage handle, so recovery is fail-closed"
                        ),
                        source: Box::new(reopen_error),
                    }
                })?;
                verify_database_authority().map_err(|authority_error| {
                    BeadsError::WithContext {
                        context: format!(
                            "WAL-certificate quarantine failed ({quarantine_error}); the restored storage handle could not be verified, so recovery is fail-closed"
                        ),
                        source: Box::new(authority_error),
                    }
                })?;
                restored_storage.set_workflow_policy(preserved_workflow_policy);
                if let Some(attribution) = preserved_attribution {
                    restored_storage.set_pending_event_attribution(attribution);
                }
                self.storage = restored_storage;
                self.database_transition_failed_closed = false;
                return Err(quarantine_error);
            }
        };

        let quarantined_paths = quarantined
            .iter()
            .map(|(_, backup)| backup.clone())
            .collect::<Vec<_>>();

        match reopen_storage() {
            Ok(mut storage) => {
                verify_database_authority().map_err(|authority_error| {
                    BeadsError::WithContext {
                        context: "Quarantined stale WAL-certificate sidecars but could not verify the reopened database generation; recovery is fail-closed"
                            .to_string(),
                        source: Box::new(authority_error),
                    }
                })?;
                storage.set_workflow_policy(preserved_workflow_policy);
                if let Some(attribution) = preserved_attribution {
                    storage.set_pending_event_attribution(attribution);
                }
                self.storage = storage;
                self.database_transition_failed_closed = false;
                warn!(
                    db_path = %self.paths.db_path.display(),
                    quarantined_paths = ?quarantined_paths,
                    "Quarantined stale parallel-WAL certificate sidecars into the recovery directory; the engine will regenerate them on the next write"
                );
                Ok(quarantined_paths)
            }
            Err(first_reopen_error) => {
                if let Err(authority_error) = verify_database_authority() {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Quarantined stale WAL-certificate sidecars but the database could not be reopened ({first_reopen_error}); refusing namespace rollback because database authority changed"
                        ),
                        source: Box::new(authority_error),
                    });
                }

                let rollback_error =
                    rollback_renamed_paths(&quarantined, "stale WAL-certificate quarantine").err();
                if let Err(authority_error) = verify_database_authority() {
                    return Err(BeadsError::WithContext {
                        context: format!(
                            "Database authority changed while rolling back stale WAL-certificate quarantine after reopen failed ({first_reopen_error}); recovery is fail-closed"
                        ),
                        source: Box::new(authority_error),
                    });
                }

                let mut restored_storage = reopen_storage().map_err(|second_reopen_error| {
                    let rollback_detail = rollback_error.as_ref().map_or_else(
                        || "sidecar rollback completed".to_string(),
                        |error| format!("sidecar rollback also failed: {error}"),
                    );
                    BeadsError::WithContext {
                        context: format!(
                            "Persistent storage reopen failed after WAL-certificate quarantine ({first_reopen_error}); {rollback_detail}; the database still could not be reopened, so recovery is fail-closed"
                        ),
                        source: Box::new(second_reopen_error),
                    }
                })?;
                verify_database_authority().map_err(|authority_error| {
                    BeadsError::WithContext {
                        context: format!(
                            "Persistent storage reopened only after WAL-certificate quarantine failed ({first_reopen_error}), but its database generation could not be verified; recovery is fail-closed"
                        ),
                        source: Box::new(authority_error),
                    }
                })?;
                restored_storage.set_workflow_policy(preserved_workflow_policy);
                if let Some(attribution) = preserved_attribution {
                    restored_storage.set_pending_event_attribution(attribution);
                }
                self.storage = restored_storage;
                self.database_transition_failed_closed = false;

                let rollback_detail = rollback_error.map_or_else(
                    || "the sidecars were rolled back and persistent storage was restored"
                        .to_string(),
                    |error| {
                        format!(
                            "sidecar rollback also failed ({error}), but verified persistent storage was restored"
                        )
                    },
                );
                Err(BeadsError::WithContext {
                    context: format!(
                        "Failed to reopen persistent storage after WAL-certificate quarantine; {rollback_detail}"
                    ),
                    source: Box::new(first_reopen_error),
                })
            }
        }
    }

    pub(crate) fn verify_retained_jsonl_source_current(&self) -> Result<()> {
        let Some(jsonl_authority) = self.jsonl_write_authority.as_deref() else {
            return Ok(());
        };
        match &self.loaded_jsonl_source {
            RetainedJsonlSource::Uncaptured => Ok(()),
            RetainedJsonlSource::Missing => {
                jsonl_authority.verify_jsonl_authority()?;
                if jsonl_authority.capture_optional_target()?.is_none() {
                    Ok(())
                } else {
                    Err(BeadsError::SyncConflict {
                        message:
                            "JSONL appeared after the command captured a missing source generation"
                                .to_string(),
                    })
                }
            }
            RetainedJsonlSource::Present(source) => {
                verify_jsonl_source_snapshot_current(source, jsonl_authority)
            }
        }
    }

    pub(crate) fn adopt_published_jsonl_source(
        &mut self,
        source: Arc<JsonlSourceSnapshot>,
        owned_authority: Option<crate::sync::JsonlFamilyWriteLock>,
    ) -> Result<()> {
        if self.jsonl_write_authority.is_some() && owned_authority.is_some() {
            return Err(BeadsError::SyncConflict {
                message:
                    "Published JSONL adoption received a second authority while startup still retains one"
                        .to_string(),
            });
        }
        if self.jsonl_write_authority.is_none() {
            self.jsonl_write_authority = Some(match owned_authority {
                Some(authority) => Arc::new(authority),
                None => Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
                    &self.paths.jsonl_path,
                    self.resolved_lock_timeout,
                )?),
            });
        }
        let authority = self
            .jsonl_write_authority
            .as_deref()
            .expect("published JSONL adoption retains an authority");
        verify_jsonl_source_snapshot_current(&source, authority)?;
        self.loaded_jsonl_state = source.state_witness();
        self.loaded_jsonl_source = RetainedJsonlSource::Present(source);
        Ok(())
    }

    pub(crate) fn verify_retained_jsonl_authority(
        &self,
        expected_authority_sha256: &str,
    ) -> Result<()> {
        let authority =
            self.jsonl_write_authority
                .as_deref()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "Published JSONL adoption did not retain its JSONL-family authority"
                        .to_string(),
                })?;
        authority.verify_jsonl_authority()?;
        if authority.authority_path_sha256() != expected_authority_sha256 {
            return Err(BeadsError::SyncConflict {
                message: "Published JSONL authority differs from the committed merge intent"
                    .to_string(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn pending_recovery_dir(&self) -> Option<&Path> {
        self.pending_recovery_backup
            .as_ref()
            .map(|backup| backup.recovery_dir.as_path())
    }

    pub(crate) fn discard_pending_recovery_backup(&mut self) -> Result<()> {
        if let Some(write_authority) = self.write_authority.as_ref() {
            write_authority.finalize_database_replacement()?;
        }
        self.pending_recovery_backup = None;
        Ok(())
    }

    /// Restore the original database family after a deferred recovery prepared
    /// a fresh DB but the explicit import later failed.
    ///
    /// # Errors
    ///
    /// Returns an error if the original database family cannot be restored.
    pub(crate) fn restore_pending_recovery_backup(&mut self) -> Result<()> {
        let Some(backup_set) = self.pending_recovery_backup.clone() else {
            return Ok(());
        };
        let had_original_database_family = backup_set.had_original_database;
        let write_authority = Arc::clone(self.write_authority.as_ref().ok_or_else(|| {
            BeadsError::SyncConflict {
                message: "Restoring a deferred recovery backup requires database-family authority"
                    .to_string(),
            }
        })?);
        verify_recovery_backup_set(&backup_set).map_err(|error| BeadsError::WithContext {
            context: format!(
                "Refusing deferred recovery rollback because its verified backup set in '{}' is incomplete or changed; the live database was left untouched",
                backup_set.recovery_dir.display()
            ),
            source: Box::new(error),
        })?;
        if write_authority.database_target_authority_state()?
            != crate::sync::DatabaseTargetAuthorityState::Held
        {
            return Err(BeadsError::SyncConflict {
                message: "Refusing deferred recovery rollback because the live database is no longer the retained replacement generation; it was left untouched"
                    .to_string(),
            });
        }

        self.storage = SqliteStorage::open_memory()?;
        restore_database_family_after_failed_rebuild_before_live_staging(
            &backup_set,
            || {
                if write_authority.database_target_authority_state()?
                    == crate::sync::DatabaseTargetAuthorityState::Held
                {
                    return Ok(());
                }
                Err(BeadsError::SyncConflict {
                    message: "Refusing deferred recovery rollback because the live database changed after its storage handle was released; it was left untouched"
                        .to_string(),
                })
            },
            |staged_paths| {
                verify_staged_database_recovery_authority(
                    &backup_set,
                    staged_paths,
                    &write_authority,
                    crate::sync::DatabaseTargetAuthorityState::Held,
                )
            },
        )?;
        if had_original_database_family {
            write_authority.restore_retained_database_inode_after_authorized_replace()?;
            write_authority.verify_database_authority()?;
            let restored_storage = SqliteStorage::open_with_timeout_under_write_authority(
                &self.paths.db_path,
                self.resolved_lock_timeout,
                &write_authority,
            )
            .map_err(|reopen_err| BeadsError::WithContext {
                context: format!(
                    "Restored the original database family at '{}' but failed to reopen it",
                    self.paths.db_path.display()
                ),
                source: Box::new(reopen_err),
            })?;
            write_authority.verify_database_authority()?;
            self.storage = restored_storage;
        } else {
            write_authority.clear_database_inode_after_authorized_remove()?;
        }
        self.loaded_jsonl_state = JsonlSourceStateWitness::Missing;
        self.loaded_jsonl_source = RetainedJsonlSource::Uncaptured;
        self.auto_rebuilt = false;
        self.pending_recovery_backup = None;
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

        let history_config = self.resolved_history_config();
        let jsonl_write_authority =
            self.jsonl_write_authority
                .as_ref()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "Mutating no-DB session has no retained JSONL-family write authority"
                        .to_string(),
                })?;
        let export_config = ExportConfig {
            // `needs_flush` is deliberately NOT passed as `force` (#405):
            // doing so disabled the exporter's data-loss guards, and the
            // import path also arms `needs_flush` when a local record wins
            // over JSONL — a state in which a forced flush can destroy
            // merged issues the DB never imported. The purge_issue flow that
            // used to need force is handled by the purged-pending-export
            // marker, which the guard subtracts from its loss computation.
            force: false,
            is_default_path: self.paths.jsonl_path == self.paths.beads_dir.join("issues.jsonl"),
            beads_dir: Some(self.paths.beads_dir.clone()),
            allow_external_jsonl: self.allow_external_jsonl,
            show_progress: false,
            history: history_config,
            ..Default::default()
        };

        let expected_source = match &self.loaded_jsonl_source {
            RetainedJsonlSource::Missing => ExpectedJsonlSourceRef::Missing,
            RetainedJsonlSource::Present(source) => {
                ExpectedJsonlSourceRef::Present(source.as_ref())
            }
            RetainedJsonlSource::Uncaptured => {
                return Err(BeadsError::SyncConflict {
                    message: "Mutating no-DB session has no captured JSONL source".to_string(),
                });
            }
        };
        let (export_result, _report) = export_to_jsonl_with_policy_expected_under_authority(
            &self.storage,
            &self.paths.jsonl_path,
            &export_config,
            expected_source,
            jsonl_write_authority,
        )?;
        let persisted_snapshot = export_result.published_source_arc()?;
        self.loaded_jsonl_state = persisted_snapshot.state_witness();
        self.loaded_jsonl_source = RetainedJsonlSource::Present(persisted_snapshot);
        finalize_export_under_authority(
            &mut self.storage,
            &export_result,
            Some(&export_result.issue_hashes),
            &self.paths.jsonl_path,
            jsonl_write_authority,
        )?;

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
        if let Some(receipt) = self.storage.pending_sync_merge_receipt()? {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Committed sync merge {} is pending {:?} reconciliation; refusing auto-flush. Run `br sync --merge` first.",
                    receipt.receipt_id, receipt.phase
                ),
            });
        }

        auto_flush(
            &mut self.storage,
            &self.paths.beads_dir,
            &self.paths.jsonl_path,
            self.allow_external_jsonl,
        )?;
        Ok(())
    }

    /// Resolve a [`HistoryConfig`] honoring the merged config layer.
    ///
    /// Operators who set `sync.history_enabled: false` (or the inverted
    /// `no-history: true`) get a config with `enabled = false`, which causes
    /// [`crate::sync::history::backup_before_export`] to short-circuit instead
    /// of creating the `.br_history/` directory. See br#293.
    #[must_use]
    pub fn resolved_history_config(&self) -> crate::sync::history::HistoryConfig {
        let mut cfg = crate::sync::history::HistoryConfig::default();
        if let Some(enabled) = history_enabled_from_layer(&self.bootstrap_layer) {
            cfg.enabled = enabled;
        }
        if let Some(secs) = history_min_interval_secs_from_env() {
            cfg.min_interval_secs = secs;
        }
        cfg
    }
}

/// Open storage with a preloaded startup snapshot and support for `--no-db` mode.
///
/// # Errors
///
/// Returns an error if JSONL import or storage setup fails.
pub fn open_storage_with_startup_config(
    startup: StartupConfig,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
) -> Result<OpenStorageResult> {
    open_storage_with_owned_write_authority(startup, cli, defer_jsonl_recovery, false)
}

/// Open storage with an explicit JSONL path policy supplied by a command that
/// already validated its resolved path.
///
/// # Errors
///
/// Returns an error if JSONL import or storage setup fails.
pub(crate) fn open_storage_with_startup_config_and_jsonl_policy(
    startup: StartupConfig,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
    allow_external_jsonl: bool,
) -> Result<OpenStorageResult> {
    open_storage_with_owned_write_authority(
        startup,
        cli,
        defer_jsonl_recovery,
        allow_external_jsonl,
    )
}

fn open_storage_with_owned_write_authority(
    startup: StartupConfig,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
    allow_external_jsonl: bool,
) -> Result<OpenStorageResult> {
    let mut layers = startup.layers.clone();
    layers.push(cli.as_layer());
    let merged = ConfigLayer::merge_layers(&layers);
    let no_db = no_db_from_layer(&merged).unwrap_or(false);
    let lock_timeout = cli
        .lock_timeout
        .or_else(|| lock_timeout_from_layer(&merged))
        .or(Some(30000));

    if no_db {
        return open_storage_with_startup_config_impl(
            startup,
            cli,
            defer_jsonl_recovery,
            None,
            allow_external_jsonl,
        );
    }

    if let Some(authority) =
        cli.database_family_write_authority_for(&startup.paths.beads_dir, &startup.paths.db_path)
    {
        authority.verify_database_authority()?;
        let mut result = open_storage_with_startup_config_impl(
            startup,
            cli,
            defer_jsonl_recovery,
            Some(authority),
            allow_external_jsonl,
        )?;
        result.write_authority = Some(Arc::clone(authority));
        return Ok(result);
    }
    if cli.holds_write_lock_for(&startup.paths.beads_dir) {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Database routing changed after acquiring the write authority for {}",
                startup.paths.beads_dir.display()
            ),
        });
    }

    if cli.read_only_fast_open {
        return open_storage_with_startup_config_impl(
            startup,
            cli,
            defer_jsonl_recovery,
            None,
            allow_external_jsonl,
        );
    }

    let authority = Arc::new(
        crate::sync::blocking_database_family_write_lock_with_timeout(
            &startup.paths.beads_dir,
            &startup.paths.db_path,
            lock_timeout,
        )?,
    );
    authority.verify_database_authority()?;
    let mut result = open_storage_with_startup_config_impl(
        startup,
        cli,
        defer_jsonl_recovery,
        Some(&authority),
        allow_external_jsonl,
    )?;
    result.write_authority = Some(authority);
    Ok(result)
}

/// Open storage with a preloaded startup snapshot while a caller-held write lock
/// already serializes recovery and schema side effects.
///
/// # Errors
///
/// Returns an error if JSONL import or storage setup fails.
pub fn open_storage_with_startup_config_under_write_lock(
    startup: StartupConfig,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
    authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<OpenStorageResult> {
    authority.verify_database_authority()?;
    let planned_authority = crate::sync::database_write_authority_sha256(&startup.paths.db_path)?;
    if planned_authority != authority.authority_path_sha256() {
        return Err(BeadsError::SyncConflict {
            message: "Startup database routing does not match the held write authority".to_string(),
        });
    }
    let mut result = open_storage_with_startup_config_impl(
        startup,
        cli,
        defer_jsonl_recovery,
        Some(authority),
        false,
    )?;
    result.write_authority = Some(Arc::clone(authority));
    Ok(result)
}

fn open_sqlite_storage_for_startup(
    beads_dir: &Path,
    paths: &ConfigPaths,
    lock_timeout: Option<u64>,
    bootstrap_layer: &ConfigLayer,
    options: SqliteStartupOpenOptions<'_>,
) -> Result<(
    SqliteRecoveryOpenResult,
    Option<Arc<crate::sync::DatabaseFamilyWriteLock>>,
)> {
    if options.defer_jsonl_recovery {
        let database_was_missing = options
            .write_authority
            .map(|authority| authority.bind_database_inode_for_mutation())
            .transpose()?
            .unwrap_or(false);
        let result = if database_was_missing && paths.jsonl_path.is_file() {
            open_when_db_file_is_missing(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                options.allow_external_jsonl,
                JsonlRecoveryStrategy::DeferToExplicitImport,
                options.write_authority,
            )?
        } else {
            open_sqlite_storage_with_deferred_jsonl_recovery(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                options.allow_external_jsonl,
                options.write_authority,
            )?
        };
        Ok((result, None))
    } else if options.read_only_fast_open {
        match SqliteStorage::open_current_read_only(&paths.db_path) {
            Ok(Some(storage)) if storage.fast_open_runtime_schema_is_compatible() => Ok((
                SqliteRecoveryOpenResult {
                    storage,
                    auto_rebuilt: false,
                    pending_recovery_backup: None,
                    recovered_jsonl: None,
                },
                None,
            )),
            Ok(Some(_) | None) => open_sqlite_storage_with_recovery_after_fast_open_miss(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                options.write_authority,
                options.allow_external_jsonl,
            ),
            Err(err) => {
                tracing::trace!(
                    error = %err,
                    "read-only fast open failed; falling back to normal storage open"
                );
                open_sqlite_storage_with_recovery_after_fast_open_miss(
                    beads_dir,
                    paths,
                    lock_timeout,
                    bootstrap_layer,
                    options.write_authority,
                    options.allow_external_jsonl,
                )
            }
        }
    } else {
        let database_was_missing = options
            .write_authority
            .map(|authority| authority.bind_database_inode_for_mutation())
            .transpose()?
            .unwrap_or(false);
        let result = if database_was_missing && paths.jsonl_path.is_file() {
            open_when_db_file_is_missing(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                options.allow_external_jsonl,
                JsonlRecoveryStrategy::RebuildFromJsonl,
                options.write_authority,
            )?
        } else {
            open_sqlite_storage_with_recovery(
                beads_dir,
                paths,
                lock_timeout,
                bootstrap_layer,
                options.allow_external_jsonl,
                options.write_authority,
            )?
        };
        Ok((result, None))
    }
}

#[allow(clippy::too_many_lines)]
fn open_storage_with_startup_config_impl(
    startup: StartupConfig,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
    write_authority: Option<&Arc<crate::sync::DatabaseFamilyWriteLock>>,
    explicit_allow_external_jsonl: bool,
) -> Result<OpenStorageResult> {
    let StartupConfig {
        paths,
        layers: startup_layers,
        ..
    } = startup;
    let beads_dir = paths.beads_dir.clone();
    let cli_layer = cli.as_layer();

    let mut all_layers = startup_layers.clone();
    all_layers.push(cli_layer);
    let merged_layer = ConfigLayer::merge_layers(&all_layers);

    let no_db = no_db_from_layer(&merged_layer).unwrap_or(false);
    let allow_external_jsonl = explicit_allow_external_jsonl
        || implicit_external_jsonl_allowed(&beads_dir, &paths.db_path, &paths.jsonl_path);

    let resolved_lock_timeout = cli
        .lock_timeout
        .or_else(|| lock_timeout_from_layer(&merged_layer))
        .or(Some(30000));

    if no_db {
        let mut storage = SqliteStorage::open_memory()?;
        validate_sync_path_with_external(&paths.jsonl_path, &beads_dir, allow_external_jsonl)?;
        let jsonl_write_authority = if cli.no_db_write_intent {
            let parent = paths.jsonl_path.parent().ok_or_else(|| {
                BeadsError::Config("Resolved no-DB JSONL path has no parent".to_string())
            })?;
            fs::create_dir_all(parent)?;
            let authority = Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
                &paths.jsonl_path,
                resolved_lock_timeout,
            )?);
            authority.verify_jsonl_authority()?;
            Some(authority)
        } else {
            None
        };
        let loaded_jsonl_snapshot = capture_optional_jsonl_source(&paths.jsonl_path)?;
        let prefix = match loaded_jsonl_snapshot.as_ref() {
            Some(source) => {
                resolve_bootstrap_issue_prefix_snapshot(&merged_layer, &beads_dir, source)?
            }
            None => resolve_bootstrap_issue_prefix_without_jsonl(&merged_layer, &beads_dir)?,
        };
        storage.set_config("issue_prefix", &prefix)?;

        // Prefix inference, every validation pass, canonical hashing, and the
        // imported rows all consume one immutable byte snapshot. Retain its
        // exact raw witness so a later no-DB flush rejects even whitespace-only
        // or missing-vs-empty source changes.
        let loaded_jsonl_state = loaded_jsonl_snapshot.as_ref().map_or(
            JsonlSourceStateWitness::Missing,
            JsonlSourceSnapshot::state_witness,
        );
        if let Some(source) = loaded_jsonl_snapshot.as_ref() {
            let mut import_config = import_config_for_resolved_jsonl(
                &beads_dir,
                &paths.db_path,
                &paths.jsonl_path,
                allow_external_jsonl,
            );
            import_config.skip_prefix_validation = true;
            import_from_jsonl_snapshot(&mut storage, source, &import_config, Some(&prefix))?;
        }
        if let Some(authority) = jsonl_write_authority.as_ref() {
            authority.verify_jsonl_authority()?;
        }

        let workflow = crate::close_policy::load_for_beads_dir(&beads_dir)?.workflow;
        storage.set_workflow_policy(workflow);

        let loaded_jsonl_source = loaded_jsonl_snapshot
            .map_or(RetainedJsonlSource::Missing, |source| {
                RetainedJsonlSource::Present(Arc::new(source))
            });
        Ok(OpenStorageResult {
            storage,
            paths,
            no_db,
            write_authority: None,
            jsonl_write_authority,
            auto_rebuilt: false,
            allow_external_jsonl,
            startup_layers,
            bootstrap_layer: merged_layer,
            resolved_lock_timeout,
            loaded_jsonl_state,
            loaded_jsonl_source,
            pending_recovery_backup: None,
            database_transition_failed_closed: false,
        })
    } else {
        let (mut sqlite_open, owned_write_authority) = open_sqlite_storage_for_startup(
            &beads_dir,
            &paths,
            resolved_lock_timeout,
            &merged_layer,
            SqliteStartupOpenOptions {
                defer_jsonl_recovery,
                read_only_fast_open: cli.read_only_fast_open,
                write_authority,
                allow_external_jsonl,
            },
        )?;
        if let Some(authority) = owned_write_authority.as_ref().or(write_authority) {
            authority.verify_database_authority()?;
            sqlite_open
                .storage
                .attach_write_authority(Arc::clone(authority));
        }
        let workflow = crate::close_policy::load_for_beads_dir(&beads_dir)?.workflow;
        sqlite_open.storage.set_workflow_policy(workflow);
        let (loaded_jsonl_state, loaded_jsonl_source, jsonl_write_authority) =
            match sqlite_open.recovered_jsonl {
                Some(recovered) => {
                    let state = recovered.source.state_witness();
                    (
                        state,
                        RetainedJsonlSource::Present(recovered.source),
                        Some(recovered.authority),
                    )
                }
                None => (
                    JsonlSourceStateWitness::Missing,
                    RetainedJsonlSource::Uncaptured,
                    None,
                ),
            };
        Ok(OpenStorageResult {
            storage: sqlite_open.storage,
            paths,
            no_db,
            write_authority: owned_write_authority,
            jsonl_write_authority,
            auto_rebuilt: sqlite_open.auto_rebuilt,
            allow_external_jsonl,
            startup_layers,
            bootstrap_layer: merged_layer,
            resolved_lock_timeout,
            loaded_jsonl_state,
            loaded_jsonl_source,
            pending_recovery_backup: sqlite_open.pending_recovery_backup,
            database_transition_failed_closed: false,
        })
    }
}

/// Open storage with CLI overrides and support for `--no-db` mode.
///
/// # Errors
///
/// Returns an error if configuration loading, JSONL import, or storage setup fails.
fn open_storage_with_cli_impl(
    beads_dir: &Path,
    cli: &CliOverrides,
    defer_jsonl_recovery: bool,
) -> Result<OpenStorageResult> {
    let startup = load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    open_storage_with_startup_config(startup, cli, defer_jsonl_recovery)
}

pub fn open_storage_with_cli(beads_dir: &Path, cli: &CliOverrides) -> Result<OpenStorageResult> {
    open_storage_with_cli_impl(beads_dir, cli, false)
}

pub fn open_storage_with_cli_deferred_jsonl_recovery(
    beads_dir: &Path,
    cli: &CliOverrides,
) -> Result<OpenStorageResult> {
    open_storage_with_cli_impl(beads_dir, cli, true)
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

/// Check merged config for `sync.history_enabled` (positive) or legacy `no-history` (inverted).
///
/// Priority order (highest first):
/// 1. `sync.history_enabled` / `sync.history-enabled` / `sync.history.enabled` — canonical positive key
/// 2. `no-history` / `no_history` / `no.history` — inverted convenience key
///
/// The canonical `sync.history_enabled: false` means "disable `.br_history/` backups".
/// `no-history: true` means the same thing. Returns `None` when no key is set so the
/// caller can keep the default-enabled behavior unchanged.
///
/// This is the storage-policy switch requested in
/// <https://github.com/Dicklesworthstone/beads_rust/issues/293> — operators who
/// want `issues.jsonl` to be the single durable state file can flip this and
/// stop the `.br_history/` directory from being created.
/// Resolve an override for the `.br_history` snapshot throttle (#313) from the
/// `BR_HISTORY_MIN_INTERVAL_SECS` environment variable.
///
/// The throttle collapses bursts of `br` mutations into at most one snapshot per
/// interval (default 5s — see [`crate::sync::history::HistoryConfig`]). Set this
/// to `0` to disable the throttle (snapshot on every export), or to a larger
/// value to snapshot less often. Returns `None` when unset/unparsable so the
/// default applies.
#[must_use]
pub fn history_min_interval_secs_from_env() -> Option<u64> {
    std::env::var("BR_HISTORY_MIN_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

#[must_use]
pub fn history_enabled_from_layer(layer: &ConfigLayer) -> Option<bool> {
    if let Some(v) = get_startup_value(
        layer,
        &[
            "sync.history_enabled",
            "sync.history-enabled",
            "sync.history.enabled",
        ],
    )
    .and_then(|value| parse_bool(value))
    {
        return Some(v);
    }
    get_startup_value(layer, &["no-history", "no_history", "no.history"])
        .and_then(|value| parse_bool(value))
        .map(|v| !v)
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

#[cfg(test)]
fn resolve_bootstrap_issue_prefix(
    bootstrap_layer: &ConfigLayer,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<String> {
    if let Some(prefix) = get_value(bootstrap_layer, &["issue_prefix", "issue-prefix", "prefix"]) {
        return normalize_configured_prefix(prefix);
    }

    if let Some(prefix) =
        first_prefix_from_resolved_jsonl(beads_dir, jsonl_path, allow_external_jsonl)?
    {
        return Ok(normalize_prefix(&prefix));
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

fn resolve_bootstrap_issue_prefix_snapshot(
    bootstrap_layer: &ConfigLayer,
    beads_dir: &Path,
    source: &crate::sync::JsonlSourceSnapshot,
) -> Result<String> {
    if let Some(prefix) = get_value(bootstrap_layer, &["issue_prefix", "issue-prefix", "prefix"]) {
        return normalize_configured_prefix(prefix);
    }

    if let Some(prefix) = first_prefix_from_jsonl_snapshot(source)? {
        return Ok(normalize_prefix(&prefix));
    }

    if let Some(name) = beads_dir
        .parent()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(abbreviate_prefix(name));
    }

    Ok("br".to_string())
}

fn resolve_bootstrap_issue_prefix_without_jsonl(
    bootstrap_layer: &ConfigLayer,
    beads_dir: &Path,
) -> Result<String> {
    if let Some(prefix) = get_value(bootstrap_layer, &["issue_prefix", "issue-prefix", "prefix"]) {
        return normalize_configured_prefix(prefix);
    }

    if let Some(name) = beads_dir
        .parent()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(abbreviate_prefix(name));
    }

    Ok("br".to_string())
}

fn validate_configured_issue_prefix(layer: &ConfigLayer) -> Result<()> {
    if let Some(prefix) = get_value(layer, &["issue_prefix", "issue-prefix", "prefix"]) {
        normalize_configured_prefix(prefix)?;
    }
    Ok(())
}

fn first_prefix_from_resolved_jsonl(
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<Option<String>> {
    if !jsonl_path.is_file() {
        return Ok(None);
    }
    validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    first_prefix_from_jsonl(jsonl_path)
}

fn import_config_for_resolved_jsonl(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
    explicit_allow_external_jsonl: bool,
) -> ImportConfig {
    ImportConfig {
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl: explicit_allow_external_jsonl
            || implicit_external_jsonl_allowed(beads_dir, db_path, jsonl_path),
        show_progress: false,
        // Every caller re-ingests the workspace's own resolved sidecar
        // (rebuild/recovery/bootstrap), never a foreign JSONL. The configured
        // prefix is the default for NEW ids, not a project-wide invariant, so
        // mixed-prefix workspaces (e.g. migrated legacy `<project>-<n>` ids)
        // must round-trip untouched — matching auto-import and reconcile.
        // Validating here made every automatic recovery fail with "Prefix
        // mismatch" on such workspaces (GitHub #440).
        skip_prefix_validation: true,
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
    crate::sync::path::validate_jsonl_fd_metadata(&file, jsonl_path)?;
    first_prefix_from_jsonl_reader(std::io::BufReader::new(file))
}

pub(crate) fn first_prefix_from_jsonl_snapshot(
    source: &crate::sync::JsonlSourceSnapshot,
) -> Result<Option<String>> {
    first_prefix_from_jsonl_reader(source.reader())
}

fn first_prefix_from_jsonl_reader(reader: impl BufRead) -> Result<Option<String>> {
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigLayer {
    pub startup: HashMap<String, String>,
    pub runtime: HashMap<String, String>,
}

impl ConfigLayer {
    /// Return a value from this layer, checking runtime keys before startup keys.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.runtime
            .get(key)
            .or_else(|| self.startup.get(key))
            .map(String::as_str)
    }

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
    /// `.beads` directory whose `.write.lock` is already held by the caller.
    ///
    /// This is process-local execution state, not persisted configuration. It
    /// lets command-local storage opens reuse the startup lock kept alive by
    /// `main` instead of trying to acquire the same advisory lock again.
    pub(crate) held_write_lock_beads_dir: Option<PathBuf>,
    /// Owned, opaque lock capability. Cloning `CliOverrides` clones this `Arc`,
    /// so no copied marker can outlive the actual database-family authority.
    pub(crate) held_write_authority: Option<Arc<crate::sync::DatabaseFamilyWriteLock>>,
    /// True when the selected command can mutate a no-DB in-memory snapshot
    /// and therefore must retain JSONL-family authority for its whole life.
    pub(crate) no_db_write_intent: bool,
    pub read_only_fast_open: bool,
}

impl CliOverrides {
    pub fn mark_no_db_write_intent(&mut self, write_intent: bool) {
        self.no_db_write_intent = write_intent;
    }

    pub fn mark_database_family_lock_held(
        &mut self,
        beads_dir: &Path,
        guard: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    ) {
        self.held_write_lock_beads_dir = Some(beads_dir.to_path_buf());
        self.held_write_authority = Some(Arc::clone(guard));
    }

    /// Drop this override set's share of the database-family authority.
    ///
    /// The marker holds a live `Arc` clone of the flock guard, so a caller
    /// that releases its own guard but keeps a marked `CliOverrides` alive
    /// would silently keep the workspace lock held. Long-lived commands that
    /// only needed startup authority for a gate check (e.g. `br serve`) must
    /// clear the marker alongside dropping the guard.
    pub fn clear_database_family_lock_marker(&mut self) {
        self.held_write_lock_beads_dir = None;
        self.held_write_authority = None;
    }

    #[must_use]
    pub fn holds_write_lock_for(&self, beads_dir: &Path) -> bool {
        self.held_write_authority.is_some()
            && self.held_write_lock_beads_dir.as_deref() == Some(beads_dir)
    }

    pub(crate) fn database_family_write_authority_for(
        &self,
        beads_dir: &Path,
        database_path: &Path,
    ) -> Option<&Arc<crate::sync::DatabaseFamilyWriteLock>> {
        let authority = self.held_write_authority.as_ref()?;
        (self.holds_write_lock_for(beads_dir)
            && crate::sync::database_write_authority_sha256(database_path)
                .is_ok_and(|planned| planned == authority.authority_path_sha256())
            && crate::sync::canonical_database_authority_path(database_path)
                .is_ok_and(|canonical| canonical == authority.canonical_database_path()))
        .then_some(authority)
    }

    #[must_use]
    pub fn holds_database_family_lock_for(&self, beads_dir: &Path, database_path: &Path) -> bool {
        self.database_family_write_authority_for(beads_dir, database_path)
            .is_some()
    }

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
    load_config_with_external_jsonl_policy(beads_dir, storage, cli, false)
}

pub(crate) fn load_config_with_external_jsonl_policy(
    beads_dir: &Path,
    storage: Option<&SqliteStorage>,
    cli: &CliOverrides,
    allow_external_jsonl: bool,
) -> Result<ConfigLayer> {
    let startup = load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let allow_external_jsonl = allow_external_jsonl
        || implicit_external_jsonl_allowed(
            &startup.paths.beads_dir,
            &startup.paths.db_path,
            &startup.paths.jsonl_path,
        );
    load_config_from_startup_layers(
        &startup.layers,
        &startup.paths.beads_dir,
        &startup.paths.jsonl_path,
        allow_external_jsonl,
        JsonlConfigSource::Uncaptured,
        storage,
        cli,
    )
}

pub(crate) fn load_config_with_external_jsonl_policy_snapshot(
    beads_dir: &Path,
    storage: Option<&SqliteStorage>,
    cli: &CliOverrides,
    allow_external_jsonl: bool,
    source: &JsonlSourceSnapshot,
) -> Result<ConfigLayer> {
    let startup = load_startup_config_with_paths(beads_dir, cli.db.as_ref())?;
    let allow_external_jsonl = allow_external_jsonl
        || implicit_external_jsonl_allowed(
            &startup.paths.beads_dir,
            &startup.paths.db_path,
            source.display_path(),
        );
    load_config_from_startup_layers(
        &startup.layers,
        &startup.paths.beads_dir,
        source.display_path(),
        allow_external_jsonl,
        JsonlConfigSource::Present(source),
        storage,
        cli,
    )
}

fn load_config_from_startup_layers(
    startup_layers: &[ConfigLayer],
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
    jsonl_source: JsonlConfigSource<'_>,
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
    let inferred_prefix = match jsonl_source {
        JsonlConfigSource::Uncaptured => {
            first_prefix_from_resolved_jsonl(beads_dir, jsonl_path, allow_external_jsonl)?
        }
        JsonlConfigSource::Missing => None,
        JsonlConfigSource::Present(source) => first_prefix_from_jsonl_snapshot(source)?,
    };
    if let Some(prefix) = inferred_prefix {
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

    let merged = ConfigLayer::merge_layers(&layers);
    validate_configured_issue_prefix(&merged)?;
    Ok(merged)
}

/// Internal structure to hold startup config and paths without redundant IO.
pub struct StartupConfig {
    pub paths: ConfigPaths,
    pub layers: Vec<ConfigLayer>,
    pub merged_config: ConfigLayer,
}

const STARTUP_CACHE_VERSION: u32 = 2;
const STARTUP_CACHE_ENABLE_ENV: &str = "BR_STARTUP_CACHE";
const STARTUP_CACHE_DIR_ENV: &str = "BR_STARTUP_CACHE_DIR";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StartupCacheRecord {
    version: u32,
    key: String,
    witness: StartupCacheWitness,
    paths: ConfigPaths,
    layers: Vec<ConfigLayer>,
    merged_config: ConfigLayer,
}

impl StartupCacheRecord {
    fn into_startup(self) -> StartupConfig {
        StartupConfig {
            paths: self.paths,
            layers: self.layers,
            merged_config: self.merged_config,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StartupCacheWitness {
    db_override: Option<PathBuf>,
    env: Vec<(String, Option<String>)>,
    files: Vec<StartupFileWitness>,
}

impl StartupCacheWitness {
    fn capture(beads_dir: &Path, db_override: Option<&PathBuf>) -> Self {
        let env = startup_cache_env_witness();
        let mut files = startup_cache_watch_paths(beads_dir)
            .into_iter()
            .map(StartupFileWitness::capture)
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));

        Self {
            db_override: db_override.cloned(),
            env,
            files,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StartupFileWitness {
    path: PathBuf,
    state: StartupPathState,
}

impl StartupFileWitness {
    fn capture(path: PathBuf) -> Self {
        let state = match fs::symlink_metadata(&path) {
            Ok(metadata) => StartupPathState::present(&metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => StartupPathState::Missing,
            Err(err) => StartupPathState::Unreadable {
                kind: err.kind().to_string(),
            },
        };
        Self { path, state }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum StartupPathState {
    Missing,
    Unreadable {
        kind: String,
    },
    Present {
        kind: StartupFileKind,
        len: u64,
        modified_nanos: Option<u128>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unix: Option<StartupUnixFileWitness>,
    },
}

impl StartupPathState {
    fn present(metadata: &fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        #[cfg(unix)]
        let unix = Some(startup_unix_file_witness(metadata));
        #[cfg(not(unix))]
        let unix = None;

        Self::Present {
            kind: if file_type.is_symlink() {
                StartupFileKind::Symlink
            } else if file_type.is_file() {
                StartupFileKind::File
            } else if file_type.is_dir() {
                StartupFileKind::Directory
            } else {
                StartupFileKind::Other
            },
            len: metadata.len(),
            modified_nanos: metadata.modified().ok().and_then(|modified| {
                modified.duration_since(UNIX_EPOCH).ok().map(|duration| {
                    u128::from(duration.as_secs()) * 1_000_000_000
                        + u128::from(duration.subsec_nanos())
                })
            }),
            unix,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum StartupFileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StartupUnixFileWitness {
    dev: u64,
    ino: u64,
    mode: u32,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
}

#[cfg(unix)]
fn startup_unix_file_witness(metadata: &fs::Metadata) -> StartupUnixFileWitness {
    use std::os::unix::fs::MetadataExt;

    StartupUnixFileWitness {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mode: metadata.mode(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime_sec: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
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
    if startup_cache_enabled() {
        let cache_dir = startup_cache_dir_from_env();
        return load_startup_config_with_paths_cached_at(beads_dir, db_override, &cache_dir);
    }

    load_startup_config_with_paths_uncached(beads_dir, db_override)
}

pub(crate) fn load_startup_config_with_paths_uncached(
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

fn load_startup_config_with_paths_cached_at(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
    cache_dir: &Path,
) -> Result<StartupConfig> {
    let before = StartupCacheWitness::capture(beads_dir, db_override);
    let key = startup_cache_key(beads_dir, &before);
    let cache_path = startup_cache_path(cache_dir, &key);

    if let Some(startup) =
        try_read_startup_cache(&cache_path, &key, &before, beads_dir, db_override)
    {
        return Ok(startup);
    }

    let direct = load_startup_config_with_paths_uncached(beads_dir, db_override)?;
    let after = StartupCacheWitness::capture(beads_dir, db_override);
    if before == after {
        let record = StartupCacheRecord {
            version: STARTUP_CACHE_VERSION,
            key,
            witness: after,
            paths: direct.paths.clone(),
            layers: direct.layers.clone(),
            merged_config: direct.merged_config.clone(),
        };
        let _ = write_startup_cache_record(&cache_path, &record);
    }
    Ok(direct)
}

fn try_read_startup_cache(
    cache_path: &Path,
    key: &str,
    before: &StartupCacheWitness,
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> Option<StartupConfig> {
    let contents = fs::read_to_string(cache_path).ok()?;
    let record: StartupCacheRecord = serde_json::from_str(&contents).ok()?;
    if record.version != STARTUP_CACHE_VERSION || record.key != key || record.witness != *before {
        return None;
    }

    let after = StartupCacheWitness::capture(beads_dir, db_override);
    if after == *before {
        Some(record.into_startup())
    } else {
        None
    }
}

fn write_startup_cache_record(cache_path: &Path, record: &StartupCacheRecord) -> Result<()> {
    let Some(parent) = cache_path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(record)?;
    let tmp_path = cache_path.with_extension(format!("tmp.{}", std::process::id()));
    let mut tmp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;
    if let Err(error) = tmp_file.write_all(&bytes) {
        drop(tmp_file);
        let _ = fs::remove_file(&tmp_path);
        return Err(error.into());
    }
    drop(tmp_file);
    fs::rename(&tmp_path, cache_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

#[must_use]
fn startup_cache_enabled() -> bool {
    env::var(STARTUP_CACHE_ENABLE_ENV)
        .ok()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false)
}

fn startup_cache_dir_from_env() -> PathBuf {
    if let Some(path) = env::var_os(STARTUP_CACHE_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return path;
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return path.join("beads").join("startup");
    }
    if let Some(home) = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return home.join(".cache").join("beads").join("startup");
    }
    env::temp_dir().join("beads-startup-cache")
}

fn startup_cache_key(beads_dir: &Path, witness: &StartupCacheWitness) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"br-startup-cache-v2");
    hasher.update(beads_dir.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    if let Some(db_override) = &witness.db_override {
        hasher.update(db_override.to_string_lossy().as_bytes());
    }
    hasher.update(b"\0");
    for (key, value) in &witness.env {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        if let Some(value) = value {
            hasher.update(value.as_bytes());
        }
        hasher.update(b"\0");
    }
    hex_encode(&hasher.finalize())
}

fn startup_cache_path(cache_dir: &Path, key: &str) -> PathBuf {
    cache_dir.join(format!("startup-{key}.json"))
}

/// Doctor-facing view of one poisoned startup-cache file. Pass-4 cycle 2:
/// the cache types themselves stay private to this module; this struct is
/// the narrow surface the doctor walks.
#[derive(Debug, Clone)]
pub struct PoisonedStartupCacheFile {
    pub path: PathBuf,
    pub kind: PoisonedStartupCacheKind,
}

#[derive(Debug, Clone)]
pub enum PoisonedStartupCacheKind {
    /// The file exists but cannot be opened or read (corrupt FS, partial
    /// write, perms drift).
    Unreadable { error: String },
    /// The file is readable but doesn't parse as a `StartupCacheRecord` — the
    /// most common cause of silent cache misses with an on-disk artifact left
    /// behind. We carry a short raw excerpt so the operator (or agent) has
    /// something to triage.
    ParseError { error: String, raw_excerpt: String },
}

/// Inspect the current workspace's startup-cache file and return it if it is
/// poisoned (unreadable or unparseable). Used by `br doctor` (detector) and
/// the `--repair` quarantine fixer.
///
/// This intentionally checks only the exact cache key the production startup
/// path would read for `beads_dir` + `db_override`. Other `startup-*.json`
/// files in the cache directory may belong to unrelated workspaces and must
/// not make this workspace's doctor report noisy.
#[must_use]
pub fn doctor_inspect_startup_cache(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> Vec<PoisonedStartupCacheFile> {
    doctor_inspect_startup_cache_at(&startup_cache_dir_from_env(), beads_dir, db_override)
}

#[must_use]
pub(crate) fn doctor_inspect_startup_cache_at(
    cache_dir: &Path,
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> Vec<PoisonedStartupCacheFile> {
    let path = doctor_startup_cache_path_at(cache_dir, beads_dir, db_override);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            return vec![PoisonedStartupCacheFile {
                path,
                kind: PoisonedStartupCacheKind::Unreadable {
                    error: err.to_string(),
                },
            }];
        }
    };

    if let Err(err) = serde_json::from_str::<StartupCacheRecord>(&contents) {
        let raw_excerpt: String = contents.chars().take(256).collect();
        return vec![PoisonedStartupCacheFile {
            path,
            kind: PoisonedStartupCacheKind::ParseError {
                error: err.to_string(),
                raw_excerpt,
            },
        }];
    }

    Vec::new()
}

/// Resolved startup-cache directory, exported so the doctor's repair flow
/// can extend its `write_scopes` to include the cache dir without
/// reaching for private cache internals.
#[must_use]
pub fn doctor_startup_cache_dir() -> PathBuf {
    startup_cache_dir_from_env()
}

/// Resolved startup-cache file for the current workspace key.
#[must_use]
pub fn doctor_startup_cache_path(beads_dir: &Path, db_override: Option<&PathBuf>) -> PathBuf {
    doctor_startup_cache_path_at(&startup_cache_dir_from_env(), beads_dir, db_override)
}

#[must_use]
pub(crate) fn doctor_startup_cache_path_at(
    cache_dir: &Path,
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> PathBuf {
    let witness = StartupCacheWitness::capture(beads_dir, db_override);
    let key = startup_cache_key(beads_dir, &witness);
    startup_cache_path(cache_dir, &key)
}

fn startup_cache_env_witness() -> Vec<(String, Option<String>)> {
    let mut keys = vec![
        "BEADS_AUTO_START_DAEMON".to_string(),
        "BEADS_CACHE_DIR".to_string(),
        "BEADS_DIR".to_string(),
        "BEADS_FLUSH_DEBOUNCE".to_string(),
        "BEADS_IDENTITY".to_string(),
        "BEADS_JSONL".to_string(),
        "BEADS_REMOTE_SYNC_INTERVAL".to_string(),
        "HOME".to_string(),
    ];
    keys.extend(env::vars().filter_map(|(key, _)| key.starts_with("BD_").then_some(key)));
    keys.sort();
    keys.dedup();

    keys.into_iter()
        .map(|key| {
            let value = env::var(&key).ok();
            (key, value)
        })
        .collect()
}

fn startup_cache_watch_paths(beads_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        beads_dir.join("metadata.json"),
        beads_dir.join("config.yaml"),
        beads_dir.join("routes.jsonl"),
        beads_dir.join("redirect"),
    ];

    if let Ok(home) = env::var("HOME")
        && !home.trim().is_empty()
    {
        let home_path = PathBuf::from(home);
        let config_root = home_path.join(".config");
        paths.push(config_root.join("beads").join("config.yaml"));
        paths.push(config_root.join("bd").join("config.yaml"));
        paths.push(home_path.join(".beads").join("config.yaml"));
    }

    if let Some(project_root) = beads_dir.parent() {
        for ancestor in project_root.ancestors() {
            paths.push(ancestor.join("mayor").join("town.json"));
        }
        if let Some(town_root) = routing::find_town_root(project_root) {
            paths.push(town_root.join(".beads").join("routes.jsonl"));
        }
    }

    paths.sort();
    paths.dedup();
    paths
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
        .map_or_else(|| "br".to_string(), |prefix| normalize_prefix(&prefix));

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
    let mut db_paths = HashMap::new();

    for (name, beads_path) in external_project_beads_dirs(layer, beads_dir) {
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

/// Resolve configured external project `.beads` directories.
///
/// Projects are expected to be either a `.beads` directory or a project root
/// containing `.beads/`.
#[must_use]
pub fn external_project_beads_dirs(
    layer: &ConfigLayer,
    beads_dir: &Path,
) -> HashMap<String, PathBuf> {
    let projects = external_projects_from_layer(layer, beads_dir);
    let mut beads_dirs = HashMap::new();

    for (name, path) in projects {
        let beads_path = external_project_beads_dir(&path);

        if !beads_path.is_dir() {
            warn!(
                project = %name,
                path = %beads_path.display(),
                "External project .beads directory not found"
            );
            continue;
        }

        beads_dirs.insert(name, beads_path);
    }

    beads_dirs
}

fn external_project_beads_dir(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(is_beads_dir_name) {
        path.to_path_buf()
    } else if path.join("_beads").is_dir() {
        path.join("_beads")
    } else {
        path.join(".beads")
    }
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
            | "no-history"
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

#[must_use]
pub fn lock_timeout_from_layer(layer: &ConfigLayer) -> Option<u64> {
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
    use crate::franken_sync::Connection;
    use crate::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use tempfile::TempDir;

    struct RelationRichFixture {
        _temp: TempDir,
        storage: SqliteStorage,
        import_result: ImportResult,
    }

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

    fn mutating_no_db_cli(db: Option<PathBuf>) -> CliOverrides {
        let mut cli = CliOverrides {
            db,
            no_db: Some(true),
            ..CliOverrides::default()
        };
        cli.mark_no_db_write_intent(true);
        cli
    }

    fn relation_rich_rebuild_fixture() -> RelationRichFixture {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let now = Utc::now();
        let parent = Issue {
            id: "bd-parent".to_string(),
            title: "Parent issue".to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        let child = Issue {
            id: "bd-parent.1".to_string(),
            title: "Child issue".to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        storage
            .upsert_issue_for_import(&parent)
            .expect("insert parent");
        storage
            .upsert_issue_for_import(&child)
            .expect("insert child");
        storage
            .sync_labels_for_import(&parent.id, &["recovery".to_string()])
            .expect("sync labels");
        storage
            .sync_dependencies_for_import(
                &child.id,
                &[Dependency {
                    issue_id: child.id.clone(),
                    depends_on_id: parent.id.clone(),
                    dep_type: DependencyType::Blocks,
                    created_at: now,
                    created_by: Some("tester".to_string()),
                    metadata: None,
                    thread_id: None,
                }],
            )
            .expect("sync dependency");
        storage
            .sync_comments_for_import(
                &parent.id,
                &[Comment {
                    id: 0,
                    issue_id: parent.id.clone(),
                    author: "tester".to_string(),
                    body: "relation survives rebuild".to_string(),
                    created_at: now,
                }],
            )
            .expect("sync comment");
        storage
            .set_export_hashes(&[
                (parent.id.clone(), "parent-hash".to_string()),
                (child.id.clone(), "child-hash".to_string()),
            ])
            .expect("set export hashes");
        let blocked_cache_entries = storage
            .rebuild_blocked_cache(true)
            .expect("rebuild blocked cache");
        let child_counter_entries = storage
            .with_write_transaction(|storage| storage.rebuild_child_counters_in_tx())
            .expect("rebuild child counters");

        RelationRichFixture {
            _temp: temp,
            storage,
            import_result: ImportResult {
                imported_count: 2,
                created_count: 2,
                labels_imported: 1,
                dependencies_imported: 1,
                comments_imported: 1,
                export_hashes_recorded: 2,
                blocked_cache_entries,
                child_counter_entries,
                ..ImportResult::default()
            },
        }
    }

    fn create_malformed_blocked_cache_db(db_path: &Path) {
        let mut storage = SqliteStorage::open(db_path).expect("create setup db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed issue prefix");
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open setup db");
        crate::storage::schema::execute_batch(
            &conn,
            "DROP TABLE blocked_issues_cache;
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO config (key, value) VALUES ('issue_prefix', 'bd');",
        )
        .expect("create malformed blocked_issues_cache schema");
    }

    fn insert_duplicate_issue_prefix_config_row(db_path: &Path, value: &str) {
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open setup db");
        conn.execute(&format!(
            "INSERT INTO config (key, value) VALUES ('issue_prefix', '{}')",
            value.replace('\'', "''")
        ))
        .expect("insert duplicate issue_prefix config row");
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
    fn config_layer_get_checks_runtime_then_startup_without_canonicalizing() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("shared".to_string(), "startup".to_string());
        layer
            .runtime
            .insert("shared".to_string(), "runtime".to_string());
        layer
            .startup
            .insert("startup_only".to_string(), "startup-only".to_string());

        assert_eq!(layer.get("shared"), Some("runtime"));
        assert_eq!(layer.get("startup_only"), Some("startup-only"));
        assert_eq!(layer.get("startup-only"), None);
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
    fn metadata_load_tolerates_legacy_bd_migration_files() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        let metadata = "{\n  \"database\": \"beads.db\",\n  \"created_at\": \"2025-01-01T00:00:00Z\",\n  \"version\": 1\n}";
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.database, "beads.db");
        assert_eq!(loaded.jsonl_export, DEFAULT_JSONL_FILENAME);
    }

    #[test]
    fn metadata_load_tolerates_missing_database_field() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        fs::write(metadata_path, r#"{"jsonl_export": "issues.jsonl"}"#).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.database, DEFAULT_DB_FILENAME);
        assert_eq!(loaded.jsonl_export, "issues.jsonl");
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
        // Discovery walks every ancestor with no boundary, so this must start
        // outside any beads workspace or it finds the enclosing repo's.
        let temp = crate::util::test_helpers::isolated_temp_dir();
        // No .beads directory created

        let result =
            discover_beads_dir_with_env_and_ceiling(Some(temp.path()), None, Some(temp.path()));
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

    /// Lay out `<root>/primary` (a fake primary checkout with a `.git`
    /// directory and an initialized `.beads`) plus `<root>/wt` (a fake
    /// linked worktree whose `.git` FILE points at
    /// `primary/.git/worktrees/wt`, with a `commondir` pointer the way git
    /// writes it). Returns `(primary_beads, worktree_root)`.
    fn linked_worktree_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let primary = root.join("primary");
        let primary_beads = primary.join(".beads");
        fs::create_dir_all(&primary_beads).expect("primary beads");
        fs::write(primary_beads.join("beads.db"), b"db").expect("primary db");
        let wt_gitdir = primary.join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&wt_gitdir).expect("worktree gitdir");
        fs::write(wt_gitdir.join("commondir"), "../..\n").expect("commondir");

        let worktree = root.join("wt");
        fs::create_dir_all(&worktree).expect("worktree root");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("worktree .git file");
        (primary_beads, worktree)
    }

    #[test]
    fn discover_resolves_linked_worktree_tracked_snapshot_to_primary() {
        // GitHub #429's underlying bug: a linked worktree checks out the
        // tracked half of `.beads` (metadata.json, issues.jsonl) but no
        // database; discovery must bind to the primary checkout instead of
        // rebuilding a private database from the stale snapshot.
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let (primary_beads, worktree) = linked_worktree_fixture(temp.path());
        let wt_beads = worktree.join(".beads");
        fs::create_dir_all(&wt_beads).expect("wt beads");
        fs::write(
            wt_beads.join("metadata.json"),
            "{\"database\": \"beads.db\"}",
        )
        .expect("wt metadata");
        fs::write(wt_beads.join("issues.jsonl"), "").expect("wt jsonl");

        let discovered = discover_beads_dir(Some(&worktree)).expect("discover");
        assert_eq!(
            dunce::canonicalize(discovered).expect("canon discovered"),
            dunce::canonicalize(primary_beads).expect("canon primary")
        );
    }

    #[test]
    fn discover_keeps_linked_worktree_workspace_with_local_database() {
        // A worktree that ran `br init` (or already imported once) owns its
        // database; discovery must not rug-pull it to the primary checkout.
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let (_primary_beads, worktree) = linked_worktree_fixture(temp.path());
        let wt_beads = worktree.join(".beads");
        fs::create_dir_all(&wt_beads).expect("wt beads");
        fs::write(
            wt_beads.join("metadata.json"),
            "{\"database\": \"beads.db\"}",
        )
        .expect("wt metadata");
        fs::write(wt_beads.join("beads.db"), b"db").expect("wt db");

        let discovered = discover_beads_dir(Some(&worktree)).expect("discover");
        assert_eq!(
            dunce::canonicalize(discovered).expect("canon discovered"),
            dunce::canonicalize(wt_beads).expect("canon wt beads")
        );
    }

    #[test]
    fn discover_resolves_sibling_worktree_without_beads_to_primary() {
        // `git worktree add ../feature-x` on a project that does NOT track
        // `.beads`: the walk finds nothing, but the `.git` file identifies
        // the primary checkout.
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let (primary_beads, worktree) = linked_worktree_fixture(temp.path());

        let discovered = discover_beads_dir(Some(&worktree)).expect("discover");
        assert_eq!(
            dunce::canonicalize(discovered).expect("canon discovered"),
            dunce::canonicalize(primary_beads).expect("canon primary")
        );
    }

    #[test]
    fn discover_prefers_explicit_worktree_redirect_over_common_dir() {
        // An explicit `.beads/redirect` is the operator's routing decision;
        // the linked-worktree resolution must stand aside and let
        // `routing::follow_redirects` handle it.
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let (primary_beads, worktree) = linked_worktree_fixture(temp.path());
        let elsewhere = temp.path().join("elsewhere").join(".beads");
        fs::create_dir_all(&elsewhere).expect("elsewhere beads");
        let wt_beads = worktree.join(".beads");
        fs::create_dir_all(&wt_beads).expect("wt beads");
        fs::write(
            wt_beads.join("redirect"),
            format!("{}\n", elsewhere.display()),
        )
        .expect("redirect file");

        let discovered = discover_beads_dir(Some(&worktree)).expect("discover");
        assert_eq!(
            dunce::canonicalize(discovered).expect("canon discovered"),
            dunce::canonicalize(&elsewhere).expect("canon elsewhere")
        );
        assert_ne!(
            dunce::canonicalize(&primary_beads).ok(),
            dunce::canonicalize(&elsewhere).ok()
        );
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
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let db_override = temp.path().join("cache").join("custom.db");
        let err = discover_beads_dir_with_cli_from_and_ceiling(
            Some(&start),
            &CliOverrides {
                db: Some(db_override.clone()),
                ..CliOverrides::default()
            },
            None,
            None,
            Some(temp.path()),
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
        let temp = crate::util::test_helpers::isolated_temp_dir();
        let start = temp.path().join("nested").join("dir");
        fs::create_dir_all(&start).expect("create nested dir");

        let err = discover_beads_dir_with_cli_from_and_ceiling(
            Some(&start),
            &CliOverrides::default(),
            None,
            Some(Path::new("/tmp/not-a-beads-db")),
            Some(temp.path()),
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
            held_write_lock_beads_dir: None,
            held_write_authority: None,
            no_db_write_intent: false,
            read_only_fast_open: false,
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
    fn history_enabled_from_layer_default_returns_none() {
        let layer = ConfigLayer::default();
        assert_eq!(history_enabled_from_layer(&layer), None);
    }

    #[test]
    fn history_enabled_from_layer_canonical_disable() {
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.history_enabled", "false".to_string());
        assert_eq!(history_enabled_from_layer(&layer), Some(false));
    }

    #[test]
    fn history_enabled_from_layer_canonical_enable() {
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.history_enabled", "true".to_string());
        assert_eq!(history_enabled_from_layer(&layer), Some(true));
    }

    #[test]
    fn history_enabled_from_layer_legacy_no_history_disables() {
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "no-history", "true".to_string());
        assert_eq!(history_enabled_from_layer(&layer), Some(false));
    }

    #[test]
    fn history_enabled_from_layer_canonical_beats_legacy() {
        // sync.history_enabled=true should win even if no-history=true is present
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.history_enabled", "true".to_string());
        insert_key_value(&mut layer, "no-history", "true".to_string());
        assert_eq!(history_enabled_from_layer(&layer), Some(true));
    }

    #[test]
    fn history_enabled_from_layer_hyphen_underscore_equivalence() {
        let mut layer = ConfigLayer::default();
        insert_key_value(&mut layer, "sync.history-enabled", "false".to_string());
        assert_eq!(history_enabled_from_layer(&layer), Some(false));
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

    #[test]
    fn id_config_normalizes_issue_prefix_values() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("issue_prefix".to_string(), " Project-Name! ".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "project-name");
    }

    #[test]
    fn load_config_rejects_explicit_prefix_without_usable_characters() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(beads_dir.join("config.yaml"), "issue_prefix: '!!!'\n").expect("write config");

        let err = load_config(&beads_dir, None, &CliOverrides::default())
            .expect_err("unsupported-only configured prefix must be rejected");

        assert!(
            matches!(&err, BeadsError::Config(message) if message.contains("issue_prefix")),
            "unexpected error: {err}"
        );
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
    fn wal_certificate_reopen_failure_restores_persistent_state_before_error() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let mut storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");

        let now = Utc::now();
        let issue = Issue {
            id: "bd-wal-state".to_string(),
            title: "Persistent across WAL-certificate rollback".to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        storage_ctx
            .storage
            .create_issue(&issue, "tester")
            .expect("seed issue");
        let workflow = crate::close_policy::Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "in_progress".to_string()],
            ..crate::close_policy::Workflow::default()
        };
        storage_ctx.storage.set_workflow_policy(workflow.clone());
        let attribution = crate::storage::EventAttribution::new(
            Some("wal-agent"),
            Some("wal-harness"),
            Some("wal-model"),
            Some("wal-session"),
        );
        storage_ctx
            .storage
            .set_pending_event_attribution(attribution.clone());

        let db_path = storage_ctx.paths.db_path.clone();
        let lock_timeout = storage_ctx.resolved_lock_timeout;
        let authority = Arc::clone(
            storage_ctx
                .write_authority
                .as_ref()
                .expect("retained database authority"),
        );
        let reopen_attempts = std::cell::Cell::new(0_u8);
        let error = storage_ctx
            .quarantine_stale_wal_certificate_sidecars_and_reopen_with(
                || Ok(Vec::new()),
                || {
                    let attempt = reopen_attempts.get();
                    reopen_attempts.set(attempt + 1);
                    if attempt == 0 {
                        Err(BeadsError::Config(
                            "simulated first reopen failure".to_string(),
                        ))
                    } else {
                        SqliteStorage::open_with_timeout_under_write_authority(
                            &db_path,
                            lock_timeout,
                            &authority,
                        )
                    }
                },
                || authority.verify_database_authority(),
            )
            .expect_err("first reopen failure must be surfaced after rollback");

        assert_eq!(reopen_attempts.get(), 2, "rollback must retry the reopen");
        assert!(
            error
                .to_string()
                .contains("persistent storage was restored"),
            "unexpected transition error: {error}"
        );
        assert!(!storage_ctx.database_transition_failed_closed);
        assert_eq!(storage_ctx.storage.workflow_policy(), workflow);
        assert_eq!(
            storage_ctx.storage.pending_event_attribution_for_review(),
            attribution
        );
        let restored_issue = storage_ctx
            .storage
            .get_issue("bd-wal-state")
            .expect("query restored storage")
            .expect("seeded issue must remain in persistent storage");
        assert_eq!(
            restored_issue.title,
            "Persistent across WAL-certificate rollback"
        );
    }

    #[test]
    fn wal_certificate_unrecoverable_reopen_fails_closed_with_retry_state() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let mut storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        fs::write(&storage_ctx.paths.jsonl_path, "{}\n").expect("write JSONL witness");

        let workflow = crate::close_policy::Workflow {
            strict: true,
            statuses: vec!["open".to_string(), "blocked".to_string()],
            ..crate::close_policy::Workflow::default()
        };
        storage_ctx.storage.set_workflow_policy(workflow.clone());
        let attribution = crate::storage::EventAttribution::new(
            Some("fail-closed-agent"),
            Some("fail-closed-harness"),
            Some("fail-closed-model"),
            Some("fail-closed-session"),
        );
        storage_ctx
            .storage
            .set_pending_event_attribution(attribution.clone());

        let reopen_attempts = std::cell::Cell::new(0_u8);
        let error = storage_ctx
            .quarantine_stale_wal_certificate_sidecars_and_reopen_with(
                || Ok(Vec::new()),
                || {
                    let attempt = reopen_attempts.get() + 1;
                    reopen_attempts.set(attempt);
                    Err(BeadsError::Config(format!(
                        "simulated reopen failure {attempt}"
                    )))
                },
                || Ok(()),
            )
            .expect_err("unrecoverable reopen must fail closed");

        assert_eq!(reopen_attempts.get(), 2, "reopen must be retried once");
        assert!(
            error.to_string().contains("recovery is fail-closed"),
            "unexpected transition error: {error}"
        );
        assert!(storage_ctx.database_transition_failed_closed);
        assert_eq!(storage_ctx.storage.workflow_policy(), workflow);
        assert_eq!(
            storage_ctx.storage.pending_event_attribution_for_review(),
            attribution
        );

        let recoverable_error = BeadsError::Database(FrankenError::WalCorrupt {
            detail: "bad wal".to_string(),
        });
        assert!(
            !storage_ctx.should_attempt_jsonl_recovery(&recoverable_error),
            "the state-carrying sentinel must never become a JSONL recovery source"
        );
        let recovery_error = storage_ctx
            .recover_database_from_jsonl()
            .expect_err("direct JSONL recovery must also fail closed");
        assert!(
            recovery_error
                .to_string()
                .contains("prior database-handle transition"),
            "unexpected direct recovery error: {recovery_error}"
        );
        assert_eq!(
            storage_ctx.storage.pending_event_attribution_for_review(),
            attribution,
            "refused recovery must not consume pending attribution"
        );
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

        let prefix =
            resolve_bootstrap_issue_prefix(&bootstrap_layer, &beads_dir, &jsonl_path, false)
                .expect("prefix");
        assert_eq!(prefix, "cfg");
    }

    #[test]
    fn resolve_bootstrap_issue_prefix_normalizes_directory_name() {
        let temp = TempDir::new().expect("tempdir");
        let project_dir = temp.path().join("My_Project-Name");
        let beads_dir = project_dir.join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&jsonl_path, "").expect("write empty jsonl");

        let prefix =
            resolve_bootstrap_issue_prefix(&ConfigLayer::default(), &beads_dir, &jsonl_path, false)
                .expect("prefix");
        assert_eq!(prefix, "mpn");
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
    fn vacuum_into_reopen_failure_returns_error_without_storage_handle() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let sidecar_path = PathBuf::from(format!("{}-wal-cert", db_path.display()));
        let mut storage = SqliteStorage::open(&db_path).expect("create storage");
        let now = Utc::now();
        let issue = Issue {
            id: "bd-vacuum".to_string(),
            title: "Survives compacted reopen failure".to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };
        storage
            .create_issue(&issue, "tester")
            .expect("seed issue before compaction");
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .expect("acquire compaction authority"),
        );
        write_authority
            .bind_database_inode_for_mutation()
            .expect("bind compaction database inode");
        storage.attach_write_authority(Arc::clone(&write_authority));

        let result = compact_database_via_vacuum_into_in_place_with_reopener(
            storage,
            &db_path,
            Some(50),
            &write_authority,
            |_, _| -> Result<SqliteStorage> {
                Err(BeadsError::Config("simulated reopen failure".to_string()))
            },
            || {
                fs::write(&sidecar_path, b"pre-compaction sidecar")?;
                Ok(())
            },
            || Ok(()),
            |from, to| {
                crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io)
            },
        );

        assert!(result.is_err(), "reopen failure should be surfaced");
        let Err(err) = result else {
            return;
        };
        let err_debug = format!("{err:?}");
        assert!(
            matches!(err, BeadsError::WithContext { .. }),
            "expected contextual reopen error, got {err_debug}"
        );
        if let BeadsError::WithContext { context, source } = err {
            assert!(context.contains("Failed to reopen compacted database after VACUUM INTO"));
            assert!(source.to_string().contains("simulated reopen failure"));
        }

        let reopened = SqliteStorage::open(&db_path).expect("compacted database remains readable");
        let recovered = reopened
            .get_issue("bd-vacuum")
            .expect("query compacted database")
            .expect("seeded issue should remain in compacted database");
        assert_eq!(recovered.title, "Survives compacted reopen failure");
        assert_eq!(
            fs::read(&sidecar_path).expect("read restored pre-compaction sidecar"),
            b"pre-compaction sidecar",
            "reopen failure must restore the sidecar generation staged with the prior main database"
        );
    }

    #[cfg(unix)]
    #[test]
    fn vacuum_into_preserves_same_byte_foreign_swap_after_candidate_lock() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let retained_original = temp.path().join("retained-original.db");
        let foreign_candidate = temp.path().join("foreign.db");
        let mut storage = SqliteStorage::open(&db_path).expect("create storage");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed database");
        storage.checkpoint_full().expect("checkpoint seed database");
        let original_inode = fs::metadata(&db_path).expect("inspect original").ino();
        fs::copy(&db_path, &foreign_candidate).expect("copy same-byte foreign candidate");
        let foreign_inode = fs::metadata(&foreign_candidate)
            .expect("inspect foreign candidate")
            .ino();
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .expect("acquire compaction authority"),
        );
        write_authority
            .bind_database_inode_for_mutation()
            .expect("bind compaction database inode");
        storage.attach_write_authority(Arc::clone(&write_authority));

        let error = compact_database_via_vacuum_into_in_place_with_reopener(
            storage,
            &db_path,
            Some(50),
            &write_authority,
            SqliteStorage::open_with_timeout,
            || {
                fs::rename(&db_path, &retained_original)?;
                fs::rename(&foreign_candidate, &db_path)?;
                Ok(())
            },
            || Ok(()),
            |from, to| {
                crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io)
            },
        )
        .expect_err("same-byte foreign swap must abort compacted installation");

        assert!(
            error.to_string().contains("generation changed"),
            "unexpected authority error: {error}"
        );
        assert_eq!(
            fs::metadata(&db_path)
                .expect("inspect preserved foreign")
                .ino(),
            foreign_inode,
            "the foreign generation must return to the canonical path"
        );
        assert_eq!(
            fs::metadata(&retained_original)
                .expect("inspect retained original")
                .ino(),
            original_inode,
            "the authority-owned original inode must remain recoverable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn vacuum_into_post_adoption_failure_restores_main_and_sidecar() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let sidecar_path = PathBuf::from(format!("{}-wal-cert", db_path.display()));
        let mut storage = SqliteStorage::open(&db_path).expect("create storage");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed database");
        storage.checkpoint_full().expect("checkpoint seed database");
        let original_inode = fs::metadata(&db_path).expect("inspect original").ino();
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .expect("acquire compaction authority"),
        );
        write_authority
            .bind_database_inode_for_mutation()
            .expect("bind compaction database inode");
        storage.attach_write_authority(Arc::clone(&write_authority));

        let error = compact_database_via_vacuum_into_in_place_with_reopener(
            storage,
            &db_path,
            Some(50),
            &write_authority,
            SqliteStorage::open_with_timeout,
            || {
                fs::write(&sidecar_path, b"retained sidecar")?;
                Ok(())
            },
            || {
                Err(BeadsError::Config(
                    "simulated post-adoption validation failure".to_string(),
                ))
            },
            |from, to| {
                crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io)
            },
        )
        .expect_err("post-adoption failure must roll back the compacted install");

        assert!(
            error
                .to_string()
                .contains("simulated post-adoption validation failure"),
            "unexpected post-adoption error: {error}"
        );
        assert_eq!(
            fs::metadata(&db_path).expect("inspect restored main").ino(),
            original_inode,
            "post-adoption failure must restore the exact pre-compaction main inode"
        );
        assert_eq!(
            fs::read(&sidecar_path).expect("read restored sidecar"),
            b"retained sidecar"
        );
        assert_eq!(
            write_authority
                .database_target_authority_state()
                .expect("classify restored database"),
            crate::sync::DatabaseTargetAuthorityState::Held
        );
    }

    #[cfg(unix)]
    #[test]
    fn vacuum_into_parent_sync_failure_restores_retained_main() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("create storage");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed database");
        storage.checkpoint_full().expect("checkpoint seed database");
        let original_inode = fs::metadata(&db_path).expect("inspect original").ino();
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .expect("acquire compaction authority"),
        );
        write_authority
            .bind_database_inode_for_mutation()
            .expect("bind compaction database inode");
        storage.attach_write_authority(Arc::clone(&write_authority));

        let error = compact_database_via_vacuum_into_in_place_with_reopener(
            storage,
            &db_path,
            Some(50),
            &write_authority,
            SqliteStorage::open_with_timeout,
            || Ok(()),
            || Ok(()),
            |_, _| {
                Err(BeadsError::Config(
                    "simulated parent-directory sync failure".to_string(),
                ))
            },
        )
        .expect_err("durability-barrier failure must roll back the compacted install");

        assert!(
            error
                .to_string()
                .contains("simulated parent-directory sync failure"),
            "unexpected durability error: {error}"
        );
        assert_eq!(
            fs::metadata(&db_path).expect("inspect restored main").ino(),
            original_inode,
            "durability failure must restore the exact pre-compaction inode"
        );
        assert_eq!(
            write_authority
                .database_target_authority_state()
                .expect("classify restored database"),
            crate::sync::DatabaseTargetAuthorityState::Held
        );
    }

    #[test]
    fn deferred_recovery_restore_reopens_original_database_family() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let now = Utc::now();
        let original_issue = Issue {
            id: "bd-original".to_string(),
            title: "Original on-disk issue".to_string(),
            created_at: now,
            updated_at: now,
            ..Issue::default()
        };

        let mut storage = SqliteStorage::open(&db_path).expect("create seed db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed issue prefix");
        storage
            .create_issue(&original_issue, "tester")
            .expect("seed original issue");
        drop(storage);

        insert_duplicate_issue_prefix_config_row(&db_path, "bd");
        write_single_issue_jsonl(&jsonl_path, "bd-import", "Deferred import payload");

        let mut storage_ctx =
            open_storage_with_cli_deferred_jsonl_recovery(&beads_dir, &CliOverrides::default())
                .expect("storage");

        assert!(
            storage_ctx.pending_recovery_dir().is_some(),
            "deferred recovery should keep a restoreable backup"
        );
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-original")
                .expect("query fresh placeholder db")
                .is_none(),
            "placeholder db should not still expose the pre-recovery issue"
        );

        storage_ctx
            .restore_pending_recovery_backup()
            .expect("restore original db");

        assert!(
            storage_ctx.pending_recovery_dir().is_none(),
            "restore should clear the pending backup handle"
        );
        assert!(!storage_ctx.auto_rebuilt);

        let restored_issue = storage_ctx
            .storage
            .get_issue("bd-original")
            .expect("query restored issue")
            .expect("original issue should be readable after restore");
        assert_eq!(restored_issue.title, "Original on-disk issue");
    }

    #[test]
    fn deferred_recovery_restore_rejects_changed_verified_backup_without_touching_live_db() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let original_issue = Issue {
            id: "bd-original".to_string(),
            title: "Original on-disk issue".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ..Issue::default()
        };
        let mut storage = SqliteStorage::open(&db_path).expect("create seed db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("seed issue prefix");
        storage
            .create_issue(&original_issue, "tester")
            .expect("seed original issue");
        drop(storage);
        insert_duplicate_issue_prefix_config_row(&db_path, "bd");
        write_single_issue_jsonl(&jsonl_path, "bd-import", "Deferred import payload");

        let mut storage_ctx =
            open_storage_with_cli_deferred_jsonl_recovery(&beads_dir, &CliOverrides::default())
                .expect("prepare deferred recovery");
        let backup_path = storage_ctx
            .pending_recovery_backup
            .as_ref()
            .expect("pending recovery")
            .files
            .iter()
            .find(|(original, _)| original == &db_path)
            .map(|(_, backup)| backup.clone())
            .expect("database backup path");
        fs::write(&backup_path, b"tampered backup bytes").expect("tamper backup");
        let live_before = fs::read(&db_path).expect("read live placeholder before rejection");

        let error = storage_ctx
            .restore_pending_recovery_backup()
            .expect_err("changed verified backup must fail closed");

        assert!(
            error.to_string().contains("verified backup") && error.to_string().contains("changed"),
            "unexpected integrity error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read live placeholder after rejection"),
            live_before,
            "failed restore must not mutate the live replacement"
        );
        assert!(
            storage_ctx.pending_recovery_backup.is_some(),
            "failed restore must retain pending recovery state"
        );
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-original")
                .expect("query live placeholder")
                .is_none(),
            "failed restore must leave the fresh placeholder open"
        );
    }

    #[test]
    fn deferred_recovery_restore_for_missing_db_cleans_up_fresh_database_family() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        write_single_issue_jsonl(&jsonl_path, "bd-import", "Deferred import payload");

        let mut storage_ctx =
            open_storage_with_cli_deferred_jsonl_recovery(&beads_dir, &CliOverrides::default())
                .expect("storage");

        assert!(
            storage_ctx.pending_recovery_dir().is_some(),
            "missing-db deferred recovery should track cleanup state"
        );
        assert!(
            db_path.is_file(),
            "opening deferred recovery should create a fresh database file"
        );

        storage_ctx
            .restore_pending_recovery_backup()
            .expect("cleanup fresh db");

        assert!(
            storage_ctx.pending_recovery_dir().is_none(),
            "cleanup should clear the pending recovery handle"
        );
        assert!(!db_path.exists(), "cleanup should remove the fresh db path");
        assert!(!storage_ctx.auto_rebuilt);
    }

    #[test]
    fn open_storage_with_startup_config_uses_preloaded_paths() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let first_jsonl = beads_dir.join("first.jsonl");
        let second_jsonl = beads_dir.join("second.jsonl");
        write_single_issue_jsonl(&first_jsonl, "bd-first", "First startup snapshot");
        write_single_issue_jsonl(&second_jsonl, "bd-second", "Mutated metadata path");

        let metadata_path = beads_dir.join("metadata.json");
        fs::write(
            &metadata_path,
            r#"{"database":"beads.db","jsonl_export":"first.jsonl"}"#,
        )
        .expect("write initial metadata");

        let startup = load_startup_config_with_paths(&beads_dir, None).expect("load startup");

        fs::write(
            &metadata_path,
            r#"{"database":"beads.db","jsonl_export":"second.jsonl"}"#,
        )
        .expect("rewrite metadata");

        let cli = CliOverrides {
            no_db: Some(true),
            ..CliOverrides::default()
        };
        let storage_ctx =
            open_storage_with_startup_config(startup, &cli, false).expect("open storage");

        assert_eq!(storage_ctx.paths.jsonl_path, first_jsonl);
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-first")
                .expect("query preloaded jsonl issue")
                .is_some(),
            "preloaded startup snapshot should still import from the original JSONL path"
        );
        assert!(
            storage_ctx
                .storage
                .get_issue("bd-second")
                .expect("query mutated jsonl issue")
                .is_none(),
            "mutating metadata after startup load must not change the opened storage paths"
        );
    }

    fn startup_cache_files(cache_dir: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(cache_dir)
            .expect("read cache dir")
            .map(|entry| entry.expect("cache entry").path())
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    #[test]
    fn startup_config_cache_invalidates_metadata_changes() {
        let temp = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let first_jsonl = beads_dir.join("first.jsonl");
        let second_jsonl = beads_dir.join("second-longer-name.jsonl");
        write_single_issue_jsonl(&first_jsonl, "bd-first", "First startup snapshot");
        write_single_issue_jsonl(&second_jsonl, "bd-second", "Second startup snapshot");

        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"first.jsonl"}"#,
        )
        .expect("write first metadata");

        let first = load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("first");
        assert_eq!(first.paths.jsonl_path, first_jsonl);
        assert_eq!(startup_cache_files(cache.path()).len(), 1);

        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"second-longer-name.jsonl"}"#,
        )
        .expect("write second metadata");

        let second = load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("second");
        assert_eq!(second.paths.jsonl_path, second_jsonl);
    }

    #[test]
    fn startup_config_cache_invalidates_project_config_changes() {
        let temp = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(beads_dir.join("config.yaml"), "no-db: true\n").expect("write config");

        let first = load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("first");
        assert_eq!(no_db_from_layer(&first.merged_config), Some(true));

        fs::write(beads_dir.join("config.yaml"), "no-db: false\n").expect("rewrite config");
        let second = load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("second");
        assert_eq!(no_db_from_layer(&second.merged_config), Some(false));
    }

    #[test]
    fn startup_config_cache_rejects_hit_if_witness_changes_during_optimistic_read() {
        let temp = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .expect("write metadata");
        let before = StartupCacheWitness::capture(&beads_dir, None);
        let key = startup_cache_key(&beads_dir, &before);
        let cache_path = startup_cache_path(cache.path(), &key);

        load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("prime cache");
        assert!(cache_path.is_file(), "priming should write startup cache");

        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"mutated-after-first-witness.jsonl"}"#,
        )
        .expect("mutate metadata");

        let stale = try_read_startup_cache(&cache_path, &key, &before, &beads_dir, None);
        assert!(
            stale.is_none(),
            "second witness check must reject a torn optimistic cache read"
        );
    }

    #[test]
    fn startup_config_cache_falls_back_from_corrupt_cache() {
        let temp = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let startup = load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path())
            .expect("prime");
        assert_eq!(startup.paths.metadata, Metadata::default());

        let cache_file = startup_cache_files(cache.path())
            .into_iter()
            .next()
            .expect("cache file");
        fs::write(cache_file, "{ definitely not valid json").expect("corrupt cache");

        let fallback =
            load_startup_config_with_paths_cached_at(&beads_dir, None, cache.path()).expect("load");
        assert_eq!(fallback.paths.metadata, Metadata::default());
    }

    #[cfg(unix)]
    #[test]
    fn startup_config_cache_rejects_existing_temp_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let cache = TempDir::new().expect("cache");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let direct =
            load_startup_config_with_paths_uncached(&beads_dir, None).expect("startup config");
        let witness = StartupCacheWitness::capture(&beads_dir, None);
        let key = startup_cache_key(&beads_dir, &witness);
        let cache_path = startup_cache_path(cache.path(), &key);
        let tmp_path = cache_path.with_extension(format!("tmp.{}", std::process::id()));
        let outside_target = temp.path().join("outside-cache-target.json");
        fs::write(&outside_target, "preserve").expect("write outside target");
        symlink(&outside_target, &tmp_path).expect("create temp symlink");

        let record = StartupCacheRecord {
            version: STARTUP_CACHE_VERSION,
            key,
            witness,
            paths: direct.paths,
            layers: direct.layers,
            merged_config: direct.merged_config,
        };
        let result = write_startup_cache_record(&cache_path, &record);

        assert!(result.is_err(), "pre-existing temp symlink must fail");
        assert_eq!(
            fs::read_to_string(&outside_target).expect("read outside target"),
            "preserve",
            "startup cache temp symlink target must not receive cache bytes"
        );
        assert!(
            !cache_path.exists(),
            "failed cache write must not install cache record"
        );
        assert!(
            fs::symlink_metadata(&tmp_path)
                .expect("temp symlink metadata")
                .file_type()
                .is_symlink(),
            "rejected pre-existing temp symlink should be left untouched"
        );
    }

    #[test]
    fn startup_config_cache_witness_tracks_routes_redirects_and_db_override() {
        let temp = TempDir::new().expect("tempdir");
        let town_root = temp.path().join("town");
        let project_root = town_root.join("project");
        let beads_dir = project_root.join(".beads");
        let town_beads = town_root.join(".beads");
        fs::create_dir_all(town_root.join("mayor")).expect("create mayor");
        fs::create_dir_all(&beads_dir).expect("create project beads");
        fs::create_dir_all(&town_beads).expect("create town beads");
        fs::write(town_root.join("mayor").join("town.json"), "{}\n").expect("write town marker");

        let initial = StartupCacheWitness::capture(&beads_dir, None);
        fs::write(
            beads_dir.join("routes.jsonl"),
            r#"{"prefix":"other-","path":"../other"}"#,
        )
        .expect("write local routes");
        assert_ne!(
            initial,
            StartupCacheWitness::capture(&beads_dir, None),
            "local routes.jsonl must invalidate cached startup metadata"
        );

        let before_redirect = StartupCacheWitness::capture(&beads_dir, None);
        fs::write(beads_dir.join("redirect"), ".\n").expect("write redirect");
        assert_ne!(
            before_redirect,
            StartupCacheWitness::capture(&beads_dir, None),
            "redirect changes must invalidate cached startup metadata"
        );

        let before_town_routes = StartupCacheWitness::capture(&beads_dir, None);
        fs::write(
            town_beads.join("routes.jsonl"),
            r#"{"prefix":"town-","path":"."}"#,
        )
        .expect("write town routes");
        assert_ne!(
            before_town_routes,
            StartupCacheWitness::capture(&beads_dir, None),
            "town routes.jsonl must invalidate cached startup metadata"
        );

        let db_a = beads_dir.join("a.db");
        let db_b = beads_dir.join("b.db");
        assert_ne!(
            StartupCacheWitness::capture(&beads_dir, Some(&db_a)),
            StartupCacheWitness::capture(&beads_dir, Some(&db_b)),
            "CLI database override changes must not reuse a stale cache entry"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_file_witness_tracks_ctime_when_mtime_is_preserved() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join("config.yaml");
        fs::write(&config_path, "no-db: true\n").expect("write config");

        let file = fs::File::open(&config_path).expect("open config");
        let metadata = file.metadata().expect("metadata");
        let original_accessed = metadata.accessed().expect("accessed");
        let original_modified = metadata.modified().expect("modified");
        let before = StartupFileWitness::capture(config_path.clone());

        std::thread::sleep(std::time::Duration::from_millis(1_100));
        fs::write(&config_path, "no-db: false\n").expect("rewrite config");
        let file = fs::File::open(&config_path).expect("reopen config");
        file.set_times(
            fs::FileTimes::new()
                .set_accessed(original_accessed)
                .set_modified(original_modified),
        )
        .expect("restore mtime");

        let after = StartupFileWitness::capture(config_path);
        assert_ne!(
            before, after,
            "preserved-mtime rewrites still need to invalidate startup cache hits"
        );

        let before_parts = unix_file_witness_parts(&before.state);
        let after_parts = unix_file_witness_parts(&after.state);
        assert!(
            before_parts.is_some() && after_parts.is_some(),
            "expected Unix file witnesses"
        );
        let Some((before_modified, before_unix)) = before_parts else {
            return;
        };
        let Some((after_modified, after_unix)) = after_parts else {
            return;
        };
        assert_eq!(
            before_modified, after_modified,
            "test setup should preserve visible mtime"
        );
        assert_ne!(
            before_unix.ctime_sec, after_unix.ctime_sec,
            "ctime seconds must participate in the startup cache witness"
        );
    }

    #[cfg(unix)]
    fn unix_file_witness_parts(
        state: &StartupPathState,
    ) -> Option<(Option<u128>, &StartupUnixFileWitness)> {
        match state {
            StartupPathState::Present {
                modified_nanos,
                unix: Some(unix),
                ..
            } => Some((*modified_nanos, unix)),
            StartupPathState::Missing
            | StartupPathState::Unreadable { .. }
            | StartupPathState::Present { .. } => None,
        }
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
    fn missing_database_recovery_collision_preserves_concurrent_family() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let sidecar_path = PathBuf::from(format!("{}-wal-cert", db_path.display()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let authority = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1_000),
        )
        .expect("acquire missing database authority");

        let error = rebuild_database_family_with_backup_before_install(
            &db_path,
            &beads_dir,
            &authority,
            SuccessfulRecoveryDisposition::FinalizeImmediately,
            || {
                fs::write(&db_path, b"concurrent database generation")
                    .expect("plant concurrent database");
                fs::write(&sidecar_path, b"concurrent sidecar generation")
                    .expect("plant concurrent sidecar");
            },
            |_| Ok(()),
        )
        .expect_err("atomic install must reject the concurrent database");

        assert!(
            error.to_string().contains("left it untouched"),
            "unexpected collision error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read concurrent database"),
            b"concurrent database generation"
        );
        assert_eq!(
            fs::read(&sidecar_path).expect("read concurrent sidecar"),
            b"concurrent sidecar generation"
        );
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        assert_eq!(
            fs::read_dir(recovery_dir)
                .expect("list recovery dir")
                .count(),
            0,
            "a collided generation must not be moved into recovery"
        );
    }

    #[test]
    fn retained_database_recovery_collision_preserves_target_and_original_backup() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let sidecar_path = PathBuf::from(format!("{}-wal-cert", db_path.display()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&db_path, b"original database generation").expect("write original database");
        let authority = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1_000),
        )
        .expect("acquire existing database authority");

        let error = rebuild_database_family_with_backup_before_install(
            &db_path,
            &beads_dir,
            &authority,
            SuccessfulRecoveryDisposition::FinalizeImmediately,
            || {
                fs::write(&db_path, b"concurrent database generation")
                    .expect("plant concurrent database");
                fs::write(&sidecar_path, b"concurrent sidecar generation")
                    .expect("plant concurrent sidecar");
            },
            |_| Ok(()),
        )
        .expect_err("atomic install must reject the concurrent database");

        assert!(
            error.to_string().contains("left it untouched"),
            "unexpected collision error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read concurrent database"),
            b"concurrent database generation"
        );
        assert_eq!(
            fs::read(&sidecar_path).expect("read concurrent sidecar"),
            b"concurrent sidecar generation"
        );
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        let recovery_bytes: Vec<Vec<u8>> = fs::read_dir(recovery_dir)
            .expect("list recovery dir")
            .map(|entry| fs::read(entry.expect("recovery entry").path()).expect("read recovery"))
            .collect();
        assert!(
            recovery_bytes
                .iter()
                .any(|bytes| bytes == b"original database generation"),
            "the retained original database must remain recoverable"
        );
        assert!(
            recovery_bytes
                .iter()
                .all(|bytes| bytes != b"concurrent database generation"
                    && bytes != b"concurrent sidecar generation"),
            "concurrent family bytes must never be moved into recovery"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_same_byte_staged_backup_inode_swap_before_fresh_install() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        let retained_original = beads_dir.join("retained-exact-original.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&db_path, b"same recovery bytes").expect("write original database");
        let original_inode = fs::metadata(&db_path).expect("inspect original").ino();
        let authority = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1_000),
        )
        .expect("acquire existing database authority");

        let error = rebuild_database_family_with_backup_before_install(
            &db_path,
            &beads_dir,
            &authority,
            SuccessfulRecoveryDisposition::FinalizeImmediately,
            || {
                let staged_database = fs::read_dir(&recovery_dir)
                    .expect("list recovery dir")
                    .filter_map(std::result::Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("beads.db."))
                            && path.extension().is_some_and(|extension| extension == "bak")
                    })
                    .expect("find staged main database");
                fs::rename(&staged_database, &retained_original)
                    .expect("retain exact authority-owned backup inode");
                fs::copy(&retained_original, &staged_database)
                    .expect("plant same-byte foreign backup inode");
            },
            |_| -> Result<()> {
                panic!("fresh installation must not run after staged-backup identity changes")
            },
        )
        .expect_err("same-byte staged backup swap must fail exact authority verification");

        assert!(
            error.to_string().contains("generation changed"),
            "unexpected staged-backup authority error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read preserved same-byte foreign database"),
            b"same recovery bytes"
        );
        assert_ne!(
            fs::metadata(&db_path)
                .expect("inspect preserved foreign database")
                .ino(),
            original_inode,
            "the same-byte foreign inode must not be mistaken for the retained original"
        );
        assert_eq!(
            fs::metadata(&retained_original)
                .expect("inspect retained exact original")
                .ino(),
            original_inode,
            "the exact authority-owned original must remain recoverable"
        );
    }

    #[test]
    fn missing_database_failed_rebuild_restores_orphaned_sidecars() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let sidecar_path = PathBuf::from(format!("{}-wal-cert", db_path.display()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&sidecar_path, b"orphaned sidecar").expect("write orphaned sidecar");
        let authority = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1_000),
        )
        .expect("acquire missing database authority");

        let error = rebuild_database_family_with_backup(
            &db_path,
            &beads_dir,
            &authority,
            SuccessfulRecoveryDisposition::FinalizeImmediately,
            |_| -> Result<()> { Err(BeadsError::Config("forced rebuild failure".to_string())) },
        )
        .expect_err("forced rebuild must fail");

        assert!(error.to_string().contains("forced rebuild failure"));
        assert!(!db_path.exists(), "fresh placeholder must be staged out");
        assert_eq!(
            fs::read(sidecar_path).expect("read restored sidecar"),
            b"orphaned sidecar"
        );
        authority
            .verify_database_authority()
            .expect("authority must return to the missing-database state");
    }

    #[test]
    fn missing_db_recovery_quarantines_orphaned_fsqlite_sidecars() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        write_single_issue_jsonl(&jsonl_path, "bd-sidecars", "Recovered after sidecars");

        let orphan_sidecars: &[(&str, &[u8])] = &[
            ("-wal-cert", b"orphan-wal-cert"),
            ("-wal-cert-head", b"orphan-wal-cert-head"),
            ("-fsqlite-ns-gate", b"orphan-ns-gate"),
            ("-fsqlite-ns-use", b"orphan-ns-use"),
        ];
        for (suffix, sentinel) in orphan_sidecars {
            let path = PathBuf::from(format!("{}{suffix}", db_path.display()));
            fs::write(path, sentinel).expect("write orphaned engine sidecar");
        }

        let storage_ctx =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect("storage");
        let issue = storage_ctx
            .storage
            .get_issue("bd-sidecars")
            .expect("query issue")
            .expect("issue should exist after rebuild");
        assert_eq!(issue.title, "Recovered after sidecars");
        drop(storage_ctx);

        let recovery_dir = beads_dir.join(RECOVERY_DIR_NAME);
        for (suffix, sentinel) in orphan_sidecars {
            let original_name = format!("beads.db{suffix}");
            let backups: Vec<_> = fs::read_dir(&recovery_dir)
                .expect("list recovery dir")
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with(&format!("{original_name}."))
                                && Path::new(name)
                                    .extension()
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("bak"))
                        })
                })
                .collect();
            let sentinel_backups: Vec<_> = backups
                .iter()
                .filter(|path| fs::read(path).is_ok_and(|bytes| bytes == *sentinel))
                .collect();
            assert_eq!(
                sentinel_backups.len(),
                1,
                "original {original_name} sentinel backup count among {backups:?}"
            );

            let live_path = beads_dir.join(&original_name);
            if live_path.exists() {
                assert_ne!(
                    fs::read(&live_path).expect("read replacement sidecar"),
                    *sentinel,
                    "the orphaned {original_name} must not remain live"
                );
            }
        }
    }

    #[test]
    fn read_only_fast_open_miss_waits_for_write_lock_before_rebuild() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        write_single_issue_jsonl(&jsonl_path, "bd-recovered", "Recovered from JSONL only");
        let _held_lock = crate::sync::blocking_write_lock(&beads_dir).expect("hold write lock");
        let cli = CliOverrides {
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };

        let err = open_storage_with_cli(&beads_dir, &cli)
            .expect_err("read-only miss should wait for recovery lock");
        let message = err.to_string();
        assert!(
            message.contains("Timed out after 1ms waiting for write lock"),
            "{message}"
        );
        assert!(!db_path.exists(), "rebuild must not run without write lock");
    }

    #[test]
    fn read_only_fast_open_miss_reuses_caller_write_lock_before_rebuild() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        write_single_issue_jsonl(&jsonl_path, "bd-recovered", "Recovered from JSONL only");
        let held_lock = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(0),
            )
            .expect("hold database-family write lock"),
        );
        let mut cli = CliOverrides {
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };
        cli.mark_database_family_lock_held(&beads_dir, &held_lock);

        let storage_ctx = open_storage_with_cli(&beads_dir, &cli)
            .expect("caller-held write lock should not be reacquired");
        let issue = storage_ctx
            .storage
            .get_issue("bd-recovered")
            .expect("query issue")
            .expect("issue should exist after rebuild");

        assert_eq!(issue.title, "Recovered from JSONL only");
        assert!(db_path.is_file(), "database should be rebuilt from JSONL");
    }

    /// The config-layer fast-open fallback must not repair engine namespace
    /// permissions until it owns the database-family authority. Startup must
    /// also classify any pending sync saga before its first mutation, so modes
    /// that prevent that read-only verdict must fail closed without a chmod.
    #[cfg(unix)]
    #[test]
    fn read_only_fast_open_refuses_namespace_repair_before_pending_verdict() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        drop(SqliteStorage::open(&db_path).expect("initialize current schema"));

        let sidecars: Vec<_> = FSQLITE_NAMESPACE_SIDECAR_SUFFIXES
            .iter()
            .map(|suffix| PathBuf::from(format!("{}{suffix}", db_path.display())))
            .filter(|path| path.is_file())
            .collect();
        assert!(
            !sidecars.is_empty(),
            "fsqlite should create namespace sidecars for the fixture"
        );
        for sidecar in &sidecars {
            fs::set_permissions(sidecar, fs::Permissions::from_mode(0o664))
                .expect("loosen namespace sidecar mode");
        }
        let before: Vec<_> = sidecars
            .iter()
            .map(|sidecar| {
                (
                    fs::read(sidecar).expect("read namespace sidecar"),
                    fs::metadata(sidecar)
                        .expect("inspect namespace sidecar")
                        .permissions()
                        .mode(),
                )
            })
            .collect();

        let held_lock = crate::sync::blocking_write_lock(&beads_dir)
            .expect("hold competing database-family lock");
        let blocked_cli = CliOverrides {
            lock_timeout: Some(1),
            read_only_fast_open: true,
            ..CliOverrides::default()
        };
        let blocked_error = open_storage_with_cli(&beads_dir, &blocked_cli)
            .expect_err("permission repair must wait for database-family authority");
        assert!(
            blocked_error
                .to_string()
                .contains("Timed out after 1ms waiting for write lock"),
            "unexpected blocked fallback error: {blocked_error}"
        );
        for (sidecar, (bytes, mode)) in sidecars.iter().zip(&before) {
            assert_eq!(fs::read(sidecar).expect("reread namespace sidecar"), *bytes);
            assert_eq!(
                fs::metadata(sidecar)
                    .expect("reinspect namespace sidecar")
                    .permissions()
                    .mode(),
                *mode,
                "blocked fast-open fallback must not chmod {}",
                sidecar.display()
            );
        }
        drop(held_lock);

        let error = open_storage_with_cli(
            &beads_dir,
            &CliOverrides {
                lock_timeout: Some(1_000),
                read_only_fast_open: true,
                ..CliOverrides::default()
            },
        )
        .expect_err("startup must classify pending sync state before namespace repair");
        assert!(
            error
                .to_string()
                .contains("repair would require mutation before the pending-saga verdict"),
            "unexpected fail-closed startup error: {error}"
        );
        for (sidecar, (bytes, mode)) in sidecars.iter().zip(&before) {
            assert_eq!(fs::read(sidecar).expect("reread namespace sidecar"), *bytes);
            assert_eq!(
                fs::metadata(sidecar)
                    .expect("reinspect namespace sidecar")
                    .permissions()
                    .mode(),
                *mode,
                "fail-closed pending-saga inspection must not chmod {}",
                sidecar.display()
            );
        }
    }

    #[test]
    fn read_only_fast_open_heals_runtime_incomplete_current_schema_under_authority() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        drop(SqliteStorage::open(&db_path).expect("initialize current schema"));

        let fixture = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open runtime-incomplete fixture");
        fixture
            .execute("DROP TABLE labels")
            .expect("drop required runtime table");
        fixture.close().expect("close fixture");

        let cli = CliOverrides {
            read_only_fast_open: true,
            ..CliOverrides::default()
        };
        let opened = open_storage_with_cli(&beads_dir, &cli)
            .expect("fast-open miss should heal under database-family authority");
        assert!(
            opened.storage.fast_open_runtime_schema_is_compatible(),
            "ordinary fallback must restore all current runtime schema objects"
        );
        assert_eq!(
            opened.storage.detect_recoverable_open_anomaly().unwrap(),
            None,
            "recording the repaired schema witness must not create duplicate metadata keys"
        );
    }

    #[test]
    fn cloned_cli_overrides_own_the_real_write_authority_not_a_stale_marker() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        drop(SqliteStorage::open(&db_path).expect("create database"));

        let held_lock = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(100),
            )
            .expect("hold database-family write authority"),
        );
        let mut cli = CliOverrides::default();
        cli.mark_database_family_lock_held(&beads_dir, &held_lock);
        let cloned = cli.clone();
        drop(cli);
        drop(held_lock);

        assert!(
            cloned.holds_database_family_lock_for(&beads_dir, &db_path),
            "the clone must retain an opaque owned lock capability"
        );
        assert!(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(1),
            )
            .is_err(),
            "a copied override must keep the actual authority locked"
        );
        drop(cloned);
        let reacquired = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(100),
        )
        .expect("authority must be reacquirable after the last owned capability drops");
        drop(reacquired);
    }

    #[test]
    fn rebuilt_database_postconditions_reject_issue_count_mismatch() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let storage = SqliteStorage::open(&db_path).expect("storage");
        let import_result = ImportResult {
            created_count: 1,
            ..ImportResult::default()
        };

        let err = verify_rebuilt_database_postconditions(&storage, &import_result)
            .expect_err("issue-count mismatch should fail validation");
        assert!(
            err.to_string()
                .contains("JSONL import created 1 issue rows"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_reject_orphaned_issue_references() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let storage = SqliteStorage::open(&db_path).expect("storage");
        let issue = Issue {
            id: "bd-kept".to_string(),
            title: "Kept issue".to_string(),
            ..Issue::default()
        };
        storage
            .upsert_issue_for_import(&issue)
            .expect("create issue");
        storage
            .execute_raw("PRAGMA foreign_keys = OFF")
            .expect("disable foreign keys");
        storage
            .execute_raw("INSERT INTO labels (issue_id, label) VALUES ('bd-missing', 'orphan')")
            .expect("insert orphan label");
        let import_result = ImportResult {
            created_count: 1,
            ..ImportResult::default()
        };

        let err = verify_rebuilt_database_postconditions(&storage, &import_result)
            .expect_err("orphaned label should fail validation");
        assert!(
            err.to_string().contains("labels.issue_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_allow_external_dependency_targets() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let storage = SqliteStorage::open(&db_path).expect("storage");
        let issue = Issue {
            id: "bd-kept".to_string(),
            title: "Kept issue".to_string(),
            ..Issue::default()
        };
        storage
            .upsert_issue_for_import(&issue)
            .expect("create issue");
        storage
            .execute_raw(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_by) \
                 VALUES ('bd-kept', 'external:other:capability', 'blocks', 'tester')",
            )
            .expect("insert external dependency");
        let import_result = ImportResult {
            created_count: 1,
            dependencies_imported: 1,
            ..ImportResult::default()
        };

        verify_rebuilt_database_postconditions(&storage, &import_result)
            .expect("external dependency targets should be allowed");
    }

    #[test]
    fn rebuilt_database_postconditions_accept_relation_rich_state() {
        let fixture = relation_rich_rebuild_fixture();

        verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect("relation-rich rebuild state should satisfy postconditions");
    }

    #[test]
    fn rebuilt_database_postconditions_reject_missing_labels() {
        let fixture = relation_rich_rebuild_fixture();
        fixture
            .storage
            .execute_raw("DELETE FROM labels")
            .expect("delete labels");

        let err = verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect_err("missing labels should fail validation");
        assert!(
            err.to_string().contains("labels row count mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_reject_missing_dependencies() {
        let fixture = relation_rich_rebuild_fixture();
        fixture
            .storage
            .execute_raw("DELETE FROM dependencies")
            .expect("delete dependencies");

        let err = verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect_err("missing dependencies should fail validation");
        assert!(
            err.to_string().contains("dependencies row count mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_reject_missing_comments() {
        let fixture = relation_rich_rebuild_fixture();
        fixture
            .storage
            .execute_raw("DELETE FROM comments")
            .expect("delete comments");

        let err = verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect_err("missing comments should fail validation");
        assert!(
            err.to_string().contains("comments row count mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_reject_stale_events() {
        let fixture = relation_rich_rebuild_fixture();
        fixture
            .storage
            .execute_raw(
                "INSERT INTO events (issue_id, event_type, actor) VALUES ('bd-parent', 'created', 'tester')",
            )
            .expect("insert stale event");

        let err = verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect_err("stale events should fail validation");
        assert!(
            err.to_string().contains("events row count mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rebuilt_database_postconditions_reject_child_counter_drift() {
        let fixture = relation_rich_rebuild_fixture();
        fixture
            .storage
            .execute_raw("UPDATE child_counters SET last_child = 99 WHERE parent_id = 'bd-parent'")
            .expect("drift child counter");

        let err = verify_rebuilt_database_postconditions(&fixture.storage, &fixture.import_result)
            .expect_err("child counter drift should fail validation");
        assert!(
            err.to_string()
                .contains("child_counters derived values differ"),
            "unexpected error: {err}"
        );
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

        let cli = mutating_no_db_cli(Some(db_path));
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
    fn open_storage_with_cli_no_db_validates_external_jsonl_before_hashing() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_jsonl = temp.path().join("external-store").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_jsonl).expect("create external jsonl directory");

        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("no-db".to_string(), "true".to_string());
        let startup = StartupConfig {
            paths: ConfigPaths {
                beads_dir: beads_dir.clone(),
                db_path: beads_dir.join(DEFAULT_DB_FILENAME),
                jsonl_path: external_jsonl,
                metadata: Metadata::default(),
            },
            layers: vec![layer.clone()],
            merged_config: layer,
        };

        let err = open_storage_with_startup_config_impl(
            startup,
            &CliOverrides::default(),
            false,
            None,
            false,
        )
        .expect_err("external no-db JSONL should be rejected before hashing");
        let message = err.to_string();
        assert!(
            message.contains("is outside .beads")
                || message.contains("outside the beads directory")
                || message.contains("regular file"),
            "unexpected error: {err}"
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
        assert!(
            fs::read_dir(&beads_dir)
                .expect("list read-only no-db workspace")
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".br-jsonl-write-")),
            "read-only no-db open must not create a JSONL authority sidecar"
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
    fn load_config_validates_external_jsonl_before_prefix_inference() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-store");
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");
        write_single_issue_jsonl(&external_jsonl, "bd-extcfg", "External config prefix");
        let storage = SqliteStorage::open_memory().expect("storage");

        let err = load_config_from_startup_layers(
            &[],
            &beads_dir,
            &external_jsonl,
            false,
            JsonlConfigSource::Uncaptured,
            Some(&storage),
            &CliOverrides::default(),
        )
        .expect_err("external JSONL should be rejected before prefix inference");
        assert!(
            err.to_string().contains("is outside .beads")
                || err.to_string().contains("outside the beads directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn repair_database_replay_preserves_explicit_external_jsonl_allowance() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let external_dir = temp.path().join("external-store");
        let jsonl_path = external_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        write_single_issue_jsonl(&jsonl_path, "source-extimp", "Imported from external JSONL");
        let mut bootstrap_layer = ConfigLayer::default();
        bootstrap_layer
            .runtime
            .insert("issue_prefix".to_string(), "target".to_string());
        let import_config = ImportConfig {
            allow_external_jsonl: true,
            rename_on_import: true,
            clear_duplicate_external_refs: true,
            beads_dir: Some(beads_dir.clone()),
            ..ImportConfig::default()
        };

        let jsonl_authority =
            crate::sync::blocking_jsonl_family_write_lock_with_timeout(&jsonl_path, None)
                .expect("JSONL authority");
        let source =
            crate::sync::capture_jsonl_source_snapshot(&jsonl_path).expect("JSONL snapshot");
        let (storage, import_result, _) = repair_database_from_jsonl_snapshot_with_import_config(
            &beads_dir,
            &db_path,
            None,
            &bootstrap_layer,
            import_config,
            &source,
            &jsonl_authority,
        )
        .expect("external JSONL repair replay should preserve explicit allowance");

        assert_eq!(import_result.created_count, 1);
        let ids = storage.get_all_ids().expect("query rebuilt ids");
        assert_eq!(ids.len(), 1);
        assert!(
            ids[0].starts_with("target-"),
            "rename-prefix replay should import with target prefix, got {ids:?}"
        );
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
            let conn = crate::franken_sync::Connection::open(
                snapshot_db_path.to_string_lossy().into_owned(),
            )?;
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

        let cli = mutating_no_db_cli(None);
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

        let cli = mutating_no_db_cli(Some(db_path));
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

        let cli = mutating_no_db_cli(Some(db_path));
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

        let cli = mutating_no_db_cli(Some(db_path));
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
        let sentinel_backups: Vec<_> = wal_backups
            .iter()
            .filter(|path| {
                fs::read_to_string(path.join("sentinel.txt")).is_ok_and(|text| text == "keep me")
            })
            .collect();
        assert_eq!(
            sentinel_backups.len(),
            1,
            "original wal directory sentinel backup count among {wal_backups:?}"
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

    #[cfg(unix)]
    #[test]
    fn open_storage_with_cli_refuses_recovery_for_symlinked_db_path() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external-db");
        let db_path = beads_dir.join("beads.db");
        let target_db_path = external_dir.join("target.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&external_dir).expect("create external dir");

        fs::write(&target_db_path, b"not a sqlite database").expect("write corrupt target");
        symlink(&target_db_path, &db_path).expect("symlink db path");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::write(&wal_path, b"short wal").expect("write truncated wal sidecar");
        write_single_issue_jsonl(&jsonl_path, "bd-symln1", "Symlinked DB recovery payload");

        let err =
            open_storage_with_cli(&beads_dir, &CliOverrides::default()).expect_err("should refuse");

        assert!(
            err.to_string().contains(SYMLINKED_DB_RECOVERY_ERROR_PREFIX)
                || err
                    .to_string()
                    .contains("expected a regular file, not a symlink"),
            "unexpected error: {err}"
        );
        assert!(
            fs::symlink_metadata(&db_path)
                .expect("stat db symlink")
                .file_type()
                .is_symlink(),
            "recovery refusal must leave the live DB symlink in place"
        );
        assert_eq!(
            fs::read_link(&db_path).expect("read db symlink"),
            target_db_path
        );
        assert_eq!(
            fs::read(&target_db_path).expect("read target bytes"),
            b"not a sqlite database",
            "recovery refusal must not rewrite the symlink target"
        );
        assert_eq!(
            fs::read(&wal_path).expect("read wal sidecar"),
            b"short wal",
            "recovery refusal must not quarantine sidecars for a symlinked DB path"
        );
        assert!(
            !recovery_dir_for_db_path(&db_path, &beads_dir).exists(),
            "refused recovery should not create backup artifacts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deferred_jsonl_recovery_refuses_broken_symlinked_db_path() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let missing_target = temp.path().join("offline").join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        symlink(&missing_target, &db_path).expect("symlink db path to missing target");
        write_single_issue_jsonl(
            &jsonl_path,
            "bd-symln2",
            "Deferred symlink recovery payload",
        );

        let err =
            open_storage_with_cli_deferred_jsonl_recovery(&beads_dir, &CliOverrides::default())
                .expect_err("deferred recovery should refuse");

        assert!(
            err.to_string().contains(SYMLINKED_DB_RECOVERY_ERROR_PREFIX)
                || err
                    .to_string()
                    .contains("expected a regular file, not a symlink"),
            "unexpected error: {err}"
        );
        assert!(
            fs::symlink_metadata(&db_path)
                .expect("stat db symlink")
                .file_type()
                .is_symlink(),
            "deferred recovery refusal must leave the broken symlink in place"
        );
        assert_eq!(
            fs::read_link(&db_path).expect("read db symlink"),
            missing_target
        );
        assert!(
            !missing_target.exists(),
            "deferred recovery must not materialize the missing external target"
        );
        assert!(
            !recovery_dir_for_db_path(&db_path, &beads_dir).exists(),
            "deferred refusal should not create backup artifacts"
        );
    }

    #[test]
    fn quarantine_truncated_wal_sidecar_moves_wal_and_shm_to_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db").expect("write db");
        fs::write(&wal_path, b"bad").expect("write truncated wal");
        fs::write(&shm_path, b"sidecar shared memory").expect("write shm");

        quarantine_truncated_wal_sidecar(&db_path, &beads_dir);

        assert!(
            !wal_path.exists(),
            "truncated wal should be moved out of the live database family"
        );
        assert!(
            !shm_path.exists(),
            "matching shm sidecar should be moved with the truncated wal"
        );

        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        let quarantined_paths: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("list recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .collect();
        let wal_backup = quarantined_paths
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("beads.db-wal.")
                            && Path::new(name)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("truncated-wal"))
                    })
            })
            .expect("wal quarantine artifact");
        let shm_backup = quarantined_paths
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("beads.db-shm.")
                            && Path::new(name)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("truncated-wal"))
                    })
            })
            .expect("shm quarantine artifact");

        assert_eq!(
            fs::read(wal_backup).expect("read quarantined wal"),
            b"bad",
            "quarantined wal bytes must remain inspectable"
        );
        assert_eq!(
            fs::read(shm_backup).expect("read quarantined shm"),
            b"sidecar shared memory",
            "quarantined shm bytes must remain inspectable"
        );
    }

    #[test]
    fn quarantine_truncated_wal_sidecar_leaves_zero_byte_wal_in_place() {
        // Regression for beads_rust#291. A 0-byte WAL is the documented
        // post-`PRAGMA wal_checkpoint(TRUNCATE)` resting state, which
        // SqliteStorage::Drop runs on every mutating br invocation. The
        // pre-fix heuristic quarantined that healthy hand-off as corruption
        // and flooded `.beads/.br_recovery/` with empty-file artifacts at
        // ~1 entry / 2 invocations on multi-agent repos.
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db").expect("write db");
        fs::write(&wal_path, b"").expect("write 0-byte wal");
        fs::write(&shm_path, b"live shm").expect("write shm");

        quarantine_truncated_wal_sidecar(&db_path, &beads_dir);

        assert!(wal_path.is_file(), "0-byte wal should remain live");
        assert!(shm_path.is_file(), "shm should remain live with 0-byte wal");
        assert!(
            !recovery_dir_for_db_path(&db_path, &beads_dir).exists(),
            "0-byte wal should not create a recovery quarantine"
        );
    }

    #[test]
    fn quarantine_truncated_wal_sidecar_leaves_valid_wal_family_in_place() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db").expect("write db");
        fs::write(&wal_path, [0_u8; 32]).expect("write valid-sized wal");
        fs::write(&shm_path, b"live shm").expect("write shm");

        quarantine_truncated_wal_sidecar(&db_path, &beads_dir);

        assert!(wal_path.is_file(), "valid-sized wal should remain live");
        assert!(shm_path.is_file(), "shm should remain live with valid wal");
        assert!(
            !recovery_dir_for_db_path(&db_path, &beads_dir).exists(),
            "valid-sized wal should not create a recovery quarantine"
        );
    }

    #[test]
    fn move_database_family_to_recovery_records_verified_file_backups() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(&db_path, b"db bytes").expect("write db");
        fs::write(&wal_path, b"wal bytes").expect("write wal");

        let backup_set = move_database_family_to_recovery(&db_path, &beads_dir, "fixed-stamp")
            .expect("backup set");

        assert_eq!(backup_set.files.len(), 2);
        assert_eq!(backup_set.verified_files.len(), 2);
        let db_verification = backup_set
            .verified_files
            .iter()
            .find(|verification| verification.original == db_path.display().to_string())
            .expect("db backup verification");
        assert_eq!(db_verification.kind, "file");
        assert_eq!(db_verification.size_bytes, Some(8));
        assert_eq!(
            db_verification.sha256.as_ref().map(String::len),
            Some(64),
            "file backup verification should record a sha256 digest"
        );
        assert_eq!(
            fs::read(Path::new(&db_verification.backup)).expect("read db backup"),
            b"db bytes"
        );
        assert!(
            !db_path.exists(),
            "verified backup move should remove the live DB path"
        );
    }

    #[test]
    fn verify_recovery_backup_artifact_rejects_digest_mismatch() {
        let temp = TempDir::new().expect("tempdir");
        let original = temp.path().join("beads.db");
        let backup = temp.path().join("beads.db.fixed-stamp.bak");
        fs::write(&original, b"original bytes").expect("write original");
        fs::write(&backup, b"corrupt bytes").expect("write backup");

        let expected = recovery_artifact_fingerprint(&original).expect("fingerprint");
        let err = verify_recovery_backup_artifact(&backup, &expected)
            .expect_err("mismatched backup should be rejected");

        assert!(
            err.to_string()
                .contains("Recovery backup verification failed")
                && err.to_string().contains(&backup.display().to_string()),
            "unexpected error: {err}"
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
        assert!(
            matches!(err, BeadsError::SyncConflict { .. }),
            "a pre-existing recovery target must be classified as a no-replace conflict: {err}"
        );

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
    fn recovery_move_rollback_never_replaces_a_late_foreign_path() {
        let temp = TempDir::new().expect("tempdir");
        let original = temp.path().join("beads.db");
        let renamed = temp.path().join("beads.db.fixed-stamp.bak");
        fs::write(&original, b"foreign-generation").expect("plant late foreign path");
        fs::write(&renamed, b"retained-original").expect("plant retained recovery artifact");

        let error = rollback_renamed_paths(
            &[(original.clone(), renamed.clone())],
            "test database-family backup",
        )
        .expect_err("rollback must refuse to replace a path that appeared late");

        assert!(
            error
                .to_string()
                .contains("without replacing an existing path"),
            "unexpected rollback error: {error}"
        );
        assert_eq!(
            fs::read(&original).expect("read foreign path"),
            b"foreign-generation",
            "rollback must preserve the late foreign generation"
        );
        assert_eq!(
            fs::read(&renamed).expect("read retained recovery artifact"),
            b"retained-original",
            "failed no-replace rollback must retain the original recovery artifact"
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
        let expected_backup = RecoveryArtifactFingerprint {
            kind: "file".to_string(),
            size_bytes: Some(12),
            sha256: Some("0".repeat(64)),
            symlink_target: None,
        };

        let err = restore_database_family_after_failed_rebuild(&RecoveryBackupSet {
            db_path: db_path.clone(),
            recovery_dir: recovery_dir.clone(),
            stamp: "fixed-stamp".to_string(),
            had_original_database: true,
            files: vec![(wal_path.clone(), wal_backup_file.clone())],
            verified_files: vec![recovery_backup_verification(
                &wal_path,
                &wal_backup_file,
                &expected_backup,
            )],
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

    #[test]
    fn restore_claims_and_verifies_backup_before_staging_live_database() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir).expect("create recovery dir");
        fs::write(&db_path, b"rebuilt-db").expect("write rebuilt db");

        let backup_path =
            recovery_dir.join(recovery_backup_filename(&db_path, "fixed-stamp", "bak"));
        fs::write(&backup_path, b"original-db").expect("write original backup");
        let fingerprint = recovery_artifact_fingerprint(&backup_path).expect("fingerprint backup");
        let verification = recovery_backup_verification(&db_path, &backup_path, &fingerprint);
        fs::write(&backup_path, b"substituted-backup").expect("substitute backup bytes");

        let error = restore_database_family_after_failed_rebuild(&RecoveryBackupSet {
            db_path: db_path.clone(),
            recovery_dir: recovery_dir.clone(),
            stamp: "fixed-stamp".to_string(),
            had_original_database: true,
            files: vec![(db_path.clone(), backup_path.clone())],
            verified_files: vec![verification],
        })
        .expect_err("substituted claimed backup must fail closed");

        assert!(
            error.to_string().contains("recorded fingerprint")
                && error
                    .to_string()
                    .contains("live database was left untouched"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read live database"),
            b"rebuilt-db",
            "backup verification failure must precede destructive live staging"
        );
        assert_eq!(
            fs::read(&backup_path).expect("read returned public backup"),
            b"substituted-backup",
            "failed claimed verification must return the exact claimed artifact"
        );
        let rebuild_failed = recovery_dir.join(recovery_backup_filename(
            &db_path,
            "fixed-stamp",
            "rebuild-failed",
        ));
        assert!(
            !rebuild_failed.exists(),
            "live database must not be staged before claimed backup verification"
        );
    }

    #[test]
    fn restore_late_authority_refusal_returns_claim_and_leaves_live_database() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir).expect("create recovery dir");
        fs::write(&db_path, b"retained-replacement").expect("write retained replacement");

        let backup_path =
            recovery_dir.join(recovery_backup_filename(&db_path, "fixed-stamp", "bak"));
        fs::write(&backup_path, b"original-db").expect("write original backup");
        let fingerprint = recovery_artifact_fingerprint(&backup_path).expect("fingerprint backup");
        let verification = recovery_backup_verification(&db_path, &backup_path, &fingerprint);
        let backup_set = RecoveryBackupSet {
            db_path: db_path.clone(),
            recovery_dir: recovery_dir.clone(),
            stamp: "fixed-stamp".to_string(),
            had_original_database: true,
            files: vec![(db_path.clone(), backup_path.clone())],
            verified_files: vec![verification],
        };

        let error = restore_database_family_after_failed_rebuild_with_hooks(
            &backup_set,
            || {
                Err(BeadsError::SyncConflict {
                    message: "late authority refusal".to_string(),
                })
            },
            |_| Ok(()),
        )
        .expect_err("late authority refusal must abort before live staging");

        assert!(
            error.to_string().contains("late authority refusal"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read retained replacement"),
            b"retained-replacement"
        );
        assert_eq!(
            fs::read(&backup_path).expect("read returned original backup"),
            b"original-db"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_same_byte_database_swap_after_final_authority_check() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(&db_path, b"original-db").expect("write original database");
        let authority = crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1_000),
        )
        .expect("acquire database-family authority");
        let backup_set = move_database_family_to_recovery(&db_path, &beads_dir, "fixed-stamp")
            .expect("move original database into recovery");
        authority
            .install_empty_database_replacement_and_bind()
            .expect("install retained replacement");
        fs::write(&db_path, b"same-rebuilt-bytes").expect("write retained replacement bytes");

        let foreign_candidate = beads_dir.join("foreign-database-generation");
        fs::write(&foreign_candidate, b"same-rebuilt-bytes")
            .expect("write same-byte foreign generation");
        let foreign_inode = fs::metadata(&foreign_candidate)
            .expect("inspect foreign generation")
            .ino();

        let error = restore_database_family_after_failed_rebuild_before_live_staging(
            &backup_set,
            || {
                assert_eq!(
                    authority.database_target_authority_state()?,
                    crate::sync::DatabaseTargetAuthorityState::Held,
                    "the final authority check must observe the retained replacement"
                );
                fs::rename(&foreign_candidate, &db_path)?;
                Ok(())
            },
            |staged_paths| {
                verify_staged_database_recovery_authority(
                    &backup_set,
                    staged_paths,
                    &authority,
                    crate::sync::DatabaseTargetAuthorityState::Held,
                )
            },
        )
        .expect_err("same-byte foreign source swap must fail the staged-inode check");

        assert!(
            error.to_string().contains("generation changed"),
            "unexpected staged-authority error: {error}"
        );
        assert_eq!(
            fs::metadata(&db_path)
                .expect("inspect restored foreign generation")
                .ino(),
            foreign_inode,
            "the foreign generation must be returned to the canonical path"
        );
        assert_eq!(
            fs::read(&db_path).expect("read restored foreign generation"),
            b"same-rebuilt-bytes"
        );
        let original_backup = backup_set
            .files
            .iter()
            .find_map(|(original, backup)| (original == &db_path).then_some(backup))
            .expect("original database backup path");
        assert_eq!(
            fs::read(original_backup).expect("read returned original backup"),
            b"original-db",
            "the original backup must remain public and uninstalled"
        );
    }

    #[test]
    fn restore_no_replace_preserves_database_that_appears_after_live_staging() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let recovery_dir = recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir).expect("create recovery dir");
        fs::write(&db_path, b"rebuilt-db").expect("write rebuilt database");

        let backup_path =
            recovery_dir.join(recovery_backup_filename(&db_path, "fixed-stamp", "bak"));
        fs::write(&backup_path, b"original-db").expect("write original backup");
        let fingerprint = recovery_artifact_fingerprint(&backup_path).expect("fingerprint backup");
        let verification = recovery_backup_verification(&db_path, &backup_path, &fingerprint);
        let backup_set = RecoveryBackupSet {
            db_path: db_path.clone(),
            recovery_dir: recovery_dir.clone(),
            stamp: "fixed-stamp".to_string(),
            had_original_database: true,
            files: vec![(db_path.clone(), backup_path.clone())],
            verified_files: vec![verification],
        };

        let error = restore_database_family_after_failed_rebuild_with_hooks(
            &backup_set,
            || Ok(()),
            |_| {
                fs::write(&db_path, b"foreign-generation")?;
                Ok(())
            },
        )
        .expect_err("restore must refuse to replace a newly appeared database");

        assert!(
            error.to_string().contains("preserved that live generation"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&db_path).expect("read foreign database"),
            b"foreign-generation",
            "restore must never replace a generation that appears in the final window"
        );
        assert_eq!(
            fs::read(&backup_path).expect("read returned original backup"),
            b"original-db",
            "the claimed original must return to its public backup name"
        );
        let rebuild_failed = recovery_dir.join(recovery_backup_filename(
            &db_path,
            "fixed-stamp",
            "rebuild-failed",
        ));
        assert_eq!(
            fs::read(&rebuild_failed).expect("read retained rebuilt database"),
            b"rebuilt-db",
            "the displaced rebuilt generation must remain available for diagnosis"
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
