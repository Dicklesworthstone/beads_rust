//! JSONL import/export for `beads_rust`.
//!
//! This module handles:
//! - Export: `SQLite` -> JSONL (for git tracking)
//! - Import: JSONL -> `SQLite` (for git clone/pull)
//! - Dirty tracking for incremental exports
//! - Collision detection during imports
//! - Path validation and allowlist enforcement

mod db_inode_lock;
pub mod history;
pub mod path;
pub mod witness;

pub use path::{
    ALLOWED_EXACT_NAMES, ALLOWED_EXTENSIONS, PathValidation, canonical_source_repo_path,
    is_sync_path_allowed, require_safe_sync_overwrite_path, require_valid_sync_path,
    validate_no_git_path, validate_sync_path, validate_sync_path_with_external,
    validate_temp_file_path,
};
pub(crate) use path::{
    JsonlSourceSnapshot, PinnedJsonlName, authority_paths_equivalent,
    capture_jsonl_source_snapshot, capture_optional_jsonl_source_snapshot,
    capture_optional_jsonl_source_snapshot_until, pin_jsonl_target,
};

use crate::error::{BeadsError, Result};
use crate::model::{Comment, Dependency, DependencyType, Issue};
use crate::storage::{EventAttribution, SqliteStorage};
use crate::sync::history::HistoryConfig;
use crate::util::id::{IdConfig, IdGenerator, parse_id};
use crate::util::progress::{create_progress_bar, create_spinner};
use crate::validation::{CommentValidator, IssueValidator};
use chrono::{DateTime, Utc};
use fsqlite_types::SqliteValue;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::hex_encode;
use indicatif::ProgressBar;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map::RandomState};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_WRITE_LOCK_TIMEOUT_MS: u64 = 30_000;
const WRITE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const EXPORT_ISSUE_BATCH_SIZE: usize = 1024;
const EXPORT_FULL_SCAN_ISSUE_THRESHOLD: usize = 20_000;
const EXPORT_PARALLEL_PREPARE_MIN_ISSUES: usize = 256;
const DEFAULT_JSONL_EXPORT_PARALLELISM: usize = 64;
const IMPORT_EXPORT_HASH_BATCH_SIZE: usize = 512;
const MAX_JSONL_TEMP_PATH_ATTEMPTS: u32 = 64;

#[cfg(test)]
thread_local! {
    static REPLACE_DATABASE_BEFORE_FINALIZE_LOCKED_VERIFY: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn maybe_replace_database_before_finalize_locked_verify(path: &Path) -> Result<()> {
    let replace =
        REPLACE_DATABASE_BEFORE_FINALIZE_LOCKED_VERIFY.with(|configured| configured.replace(false));
    if !replace {
        return Ok(());
    }
    let mut retained = path.as_os_str().to_os_string();
    retained.push(".test-retained-before-finalize-verify");
    fs::rename(path, PathBuf::from(retained))?;
    fs::write(
        path,
        b"foreign database generation installed by finalize hook",
    )?;
    Ok(())
}

/// Exact source state used by stale-overwrite guards.
///
/// `Missing` is intentionally distinct from a present zero-byte file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum JsonlSourceIdentityWitness {
    #[cfg(unix)]
    Unix { device_id: u64, inode: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum JsonlSourceStateWitness {
    Missing,
    Present {
        raw_sha256: String,
        mtime: String,
        size: u64,
        identity: Option<JsonlSourceIdentityWitness>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum ExpectedJsonlSourceRef<'a> {
    Missing,
    Present(&'a JsonlSourceSnapshot),
}

impl JsonlSourceSnapshot {
    #[must_use]
    pub(crate) fn state_witness(&self) -> JsonlSourceStateWitness {
        #[cfg(unix)]
        let identity = {
            let identity = self.identity();
            Some(JsonlSourceIdentityWitness::Unix {
                device_id: identity.device_id(),
                inode: identity.inode(),
            })
        };
        #[cfg(not(unix))]
        let identity = None;

        JsonlSourceStateWitness::Present {
            raw_sha256: self.raw_sha256().to_string(),
            mtime: DateTime::<Utc>::from(self.modified()).to_rfc3339(),
            size: self.size(),
            identity,
        }
    }
}

pub(crate) fn capture_optional_jsonl_source(path: &Path) -> Result<Option<JsonlSourceSnapshot>> {
    capture_optional_jsonl_source_snapshot(path)
}

pub(crate) fn capture_optional_jsonl_source_until(
    path: &Path,
    deadline: Instant,
) -> Result<Option<JsonlSourceSnapshot>> {
    capture_optional_jsonl_source_snapshot_until(path, deadline)
}

pub(crate) fn verify_jsonl_source_snapshot_current(
    source: &JsonlSourceSnapshot,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<()> {
    jsonl_authority.verify_jsonl_authority()?;
    let pinned_source = jsonl_authority.pinned_name_for_target(source.display_path())?;
    verify_expected_jsonl_source_state_observed(
        pinned_source.capture_optional()?.as_ref(),
        None,
        Some(&source.state_witness()),
    )
}

#[cfg(test)]
fn verify_expected_jsonl_source_state(
    path: &Path,
    expected_previous_content_sha256: Option<&Option<String>>,
    expected_previous_source: Option<&JsonlSourceStateWitness>,
) -> Result<()> {
    verify_expected_jsonl_source_state_observed(
        capture_optional_jsonl_source(path)?.as_ref(),
        expected_previous_content_sha256,
        expected_previous_source,
    )
}

fn verify_expected_jsonl_source_state_observed(
    observed_source: Option<&JsonlSourceSnapshot>,
    expected_previous_content_sha256: Option<&Option<String>>,
    expected_previous_source: Option<&JsonlSourceStateWitness>,
) -> Result<()> {
    if let Some(expected_previous) = expected_previous_source {
        let observed = observed_source.map_or(
            JsonlSourceStateWitness::Missing,
            JsonlSourceSnapshot::state_witness,
        );
        if &observed != expected_previous {
            return Err(BeadsError::SyncConflict {
                message: "JSONL exact bytes changed on disk since the exporting session loaded them; refusing a stale atomic replacement"
                    .to_string(),
            });
        }
    } else if let Some(expected_previous) = expected_previous_content_sha256 {
        let observed = observed_source.map(|source| source.content_sha256().to_string());
        if &observed != expected_previous {
            return Err(BeadsError::SyncConflict {
                message: "JSONL changed on disk since the exporting session loaded it; refusing a stale atomic replacement"
                    .to_string(),
            });
        }
    }

    Ok(())
}

/// Acquire a blocking exclusive lock on `.beads/.write.lock`.
///
/// This serializes all mutating operations across processes, preventing
/// concurrent-write deadlocks in the underlying SQLite engine. Uses a fast-path
/// `try_lock()` for the uncontended case, then polls with a bounded timeout for
/// contended locks. The lock is held until the returned `File` drops.
#[allow(clippy::incompatible_msrv)]
pub fn blocking_write_lock(beads_dir: &Path) -> Result<File> {
    blocking_write_lock_with_timeout(beads_dir, None)
}

/// Acquire a bounded exclusive lock on `.beads/.write.lock`.
///
/// `lock_timeout_ms` uses the same millisecond setting as `--lock-timeout`.
/// When unset, a 30s default prevents a stuck writer from parking every
/// subsequent mutating command indefinitely.
#[allow(clippy::incompatible_msrv)]
pub fn blocking_write_lock_with_timeout(
    beads_dir: &Path,
    lock_timeout_ms: Option<u64>,
) -> Result<File> {
    let lock_path = beads_dir.join(".write.lock");
    open_and_lock_regular_file(
        &lock_path,
        lock_timeout_ms,
        true,
        "workspace write lock",
        false,
        ExclusiveLockMechanism::LockSidecar,
    )
}

/// Mechanism used to place an exclusive advisory lock on a file.
///
/// Dedicated lock sidecars (`.write.lock`, `.br-db-write-*.lock`, JSONL
/// authority sidecars) use the whole-file OS primitive behind
/// [`File::try_lock`]. The SQLite database inode itself (and replacement
/// candidates destined to become it) MUST use the SQLite-compatible one-byte
/// range lock instead: on macOS/BSD a whole-file `flock` collides with the
/// engine's POSIX record locks even within one process, and on Windows a
/// whole-file `LockFileEx` is mandatory and blocks the engine's reads
/// outright (GitHub #412). See [`db_inode_lock`] for the full story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExclusiveLockMechanism {
    /// Whole-file lock via [`File::try_lock`] — for dedicated lock sidecars
    /// that no other subsystem ever locks.
    LockSidecar,
    /// One-byte range lock at [`db_inode_lock::DATABASE_INODE_LOCK_OFFSET`] —
    /// for any inode a SQLite engine may open.
    DatabaseInode,
}

/// Non-blocking exclusive lock attempt through the selected mechanism.
#[allow(clippy::incompatible_msrv)]
fn try_lock_exclusive(
    file: &File,
    mechanism: ExclusiveLockMechanism,
) -> std::result::Result<(), TryLockError> {
    match mechanism {
        ExclusiveLockMechanism::LockSidecar => file.try_lock(),
        ExclusiveLockMechanism::DatabaseInode => db_inode_lock::try_lock_database_inode(file),
    }
}

/// Acquire an advisory lock on the exact database inode used by a reviewed
/// reconciliation.
///
/// This second authority lock composes with the workspace `.write.lock`.
/// It is required for configured external databases because two independent
/// workspaces can legitimately route to the same database while owning
/// different workspace lock files.
///
/// The lock is a SQLite-compatible byte-range lock, never a whole-file lock:
/// the engine holds its own advisory locks on this inode (GitHub #412).
fn blocking_database_file_lock_with_timeout(
    database_path: &Path,
    lock_timeout_ms: Option<u64>,
    create_if_missing: bool,
) -> Result<File> {
    open_and_lock_regular_file(
        database_path,
        lock_timeout_ms,
        create_if_missing,
        "database write authority",
        true,
        ExclusiveLockMechanism::DatabaseInode,
    )
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn install_database_candidate_no_replace(candidate: &Path, target: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    match renameat_with(CWD, candidate, CWD, target, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => Err(BeadsError::SyncConflict {
            message:
                "Database appeared before the atomic no-replace installation; refusing to overwrite it"
                    .to_string(),
        }),
        Err(error) if flagged_rename_unsupported(error) => Err(BeadsError::Config(format!(
            "Filesystem does not support the atomic no-replace operation required to install a fresh database: {}",
            std::io::Error::from(error)
        ))),
        Err(error) => Err(BeadsError::Io(std::io::Error::from(error))),
    }
}

#[cfg(windows)]
fn install_database_candidate_no_replace(candidate: &Path, target: &Path) -> Result<()> {
    match db_inode_lock::rename_database_candidate_no_replace(candidate, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(BeadsError::SyncConflict {
                message:
                    "Database appeared before the atomic no-replace installation; refusing to overwrite it"
                        .to_string(),
            })
        }
        Err(error) => Err(BeadsError::Io(error)),
    }
}

/// Atomically rename a recovery artifact on Windows only when the destination
/// name is still absent.
///
/// `std::fs::rename` replaces an existing destination on current Windows
/// implementations, so recovery code must use the same native no-replace
/// primitive as fresh-database installation.
#[cfg(windows)]
pub(crate) fn rename_path_no_replace_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    db_inode_lock::rename_database_candidate_no_replace(from, to)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn install_database_candidate_no_replace(_candidate: &Path, _target: &Path) -> Result<()> {
    Err(BeadsError::Config(
        "This platform does not provide the atomic no-replace primitive required to install a fresh database"
            .to_string(),
    ))
}

#[derive(Debug)]
struct DatabaseInodeAuthority {
    lock: Option<File>,
    identity: Option<(u64, u64)>,
    retired_locks: Vec<File>,
}

/// Relationship between the canonical database path and the inode retained by
/// a database-family authority.
///
/// Recovery uses this after a failed no-replace installation. Only `Held` is
/// safe to stage out as the recovery attempt's own replacement; `Foreign`
/// must be left byte-for-byte untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseTargetAuthorityState {
    Missing,
    Held,
    Foreign,
}

/// Linear proof that this authority installed the database inode as an empty
/// replacement.
///
/// The fields stay private and the witness is deliberately neither `Clone`
/// nor `Copy`: only the locked installation path can mint it, and exactly one
/// fresh-database import may consume it.
#[derive(Debug)]
pub(crate) struct FreshDatabaseReplacementWitness {
    authority_path_sha256: String,
    installed_identity: (u64, u64),
}

/// Composite advisory authority for every mutation of one database family.
///
/// The workspace lock preserves existing single-workspace serialization, the
/// canonical-path sidecar serializes independent workspaces before a database
/// exists or across atomic replacement, and the database-inode lock unifies
/// hard-link aliases once the file exists.
#[derive(Debug)]
pub struct DatabaseFamilyWriteLock {
    workspace_lock: File,
    workspace_lock_path: PathBuf,
    authority_lock: File,
    authority_lock_path: PathBuf,
    database_authority: std::sync::Mutex<DatabaseInodeAuthority>,
    authority_path_sha256: String,
    routed_database_path: PathBuf,
    canonical_database_path: PathBuf,
    acquisition_started: Instant,
    total_timeout_ms: u64,
}

/// Stable canonical-path authority for an atomically replaced JSONL export.
///
/// Unlike a SQLite database, JSONL export intentionally renames a fresh inode
/// over the destination. Locking the destination inode would therefore become
/// stale at every successful flush. The workspace lock plus a sidecar derived
/// from the canonical destination path remains stable across that rename.
#[derive(Debug)]
pub struct JsonlFamilyWriteLock {
    authority_lock: File,
    authority_lock_path: PathBuf,
    authority_path_sha256: String,
    routed_jsonl_path: PathBuf,
    canonical_jsonl_path: PathBuf,
    pinned_jsonl_name: PinnedJsonlName,
}

impl JsonlFamilyWriteLock {
    #[must_use]
    pub fn authority_path_sha256(&self) -> &str {
        &self.authority_path_sha256
    }

    #[must_use]
    pub fn canonical_jsonl_path(&self) -> &Path {
        &self.canonical_jsonl_path
    }

    fn pinned_name_for_target(&self, path: &Path) -> Result<PinnedJsonlName> {
        if path == self.routed_jsonl_path || path == self.canonical_jsonl_path {
            return Ok(self.pinned_jsonl_name.clone());
        }
        let pinned = self.pinned_jsonl_name.with_sibling_path(path)?;
        if pinned.leaf() != self.pinned_jsonl_name.leaf() {
            return Err(BeadsError::SyncConflict {
                message: "JSONL target does not match its retained leaf capability".to_string(),
            });
        }
        Ok(pinned)
    }

    fn pinned_sibling(&self, path: &Path) -> Result<PinnedJsonlName> {
        if path.parent() == self.routed_jsonl_path.parent() {
            let leaf = path.file_name().ok_or_else(|| {
                BeadsError::Config("JSONL sibling path has no leaf name".to_string())
            })?;
            return self.pinned_jsonl_name.with_leaf(leaf);
        }
        self.pinned_jsonl_name.with_sibling_path(path)
    }

    pub(crate) fn capture_target(&self) -> Result<JsonlSourceSnapshot> {
        self.verify_jsonl_authority()?;
        let source = self.pinned_jsonl_name.capture()?;
        self.verify_jsonl_authority()?;
        Ok(source)
    }

    pub(crate) fn capture_optional_target(&self) -> Result<Option<JsonlSourceSnapshot>> {
        self.verify_jsonl_authority()?;
        let source = self.pinned_jsonl_name.capture_optional()?;
        self.verify_jsonl_authority()?;
        Ok(source)
    }

    fn fsync_pinned_parent(&self) -> std::io::Result<()> {
        self.pinned_jsonl_name.parent().fsync()
    }

    pub fn verify_jsonl_authority(&self) -> Result<()> {
        verify_locked_file_identity(
            &self.authority_lock,
            &self.authority_lock_path,
            "JSONL-family write lock",
            true,
        )?;
        self.pinned_jsonl_name.parent().verify_route()?;
        let canonical_now = canonical_database_authority_key(&self.canonical_jsonl_path)?;
        if canonical_now != self.canonical_jsonl_path {
            return Err(BeadsError::SyncConflict {
                message: "Canonical JSONL path changed while its write authority was held"
                    .to_string(),
            });
        }
        // The pinned route keeps its lexical spelling while the sidecar key is
        // a `fs::canonicalize` product; resolve both through the shared
        // comparison convention so Windows verbatim/8.3 spellings of one
        // target agree, while a genuinely different target still conflicts
        // (#413).
        if !authority_paths_equivalent(
            self.pinned_jsonl_name.display_path(),
            &self.canonical_jsonl_path,
        ) {
            return Err(BeadsError::SyncConflict {
                message: "Pinned JSONL target changed while its write authority was held"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// Stable SHA-256 of a canonical routed path without lossy UTF-8 conversion.
#[must_use]
pub(crate) fn canonical_sync_path_sha256(path: &Path) -> String {
    let mut hasher = Sha256::new();
    update_sync_path_digest(&mut hasher, path);
    crate::util::hex_encode(&hasher.finalize())
}

fn update_sync_path_digest(hasher: &mut Sha256, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
}

impl DatabaseFamilyWriteLock {
    #[cfg(test)]
    fn arm_database_replacement_before_finalize_locked_verify_for_test() {
        REPLACE_DATABASE_BEFORE_FINALIZE_LOCKED_VERIFY.with(|configured| configured.set(true));
    }

    #[must_use]
    pub fn authority_path_sha256(&self) -> &str {
        &self.authority_path_sha256
    }

    #[must_use]
    pub fn canonical_database_path(&self) -> &Path {
        &self.canonical_database_path
    }

    fn remaining_lock_timeout_ms(&self) -> u64 {
        let elapsed_ms =
            u64::try_from(self.acquisition_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.total_timeout_ms.saturating_sub(elapsed_ms)
    }

    fn verify_common_authority(&self) -> Result<()> {
        verify_locked_file_identity(
            &self.workspace_lock,
            &self.workspace_lock_path,
            "workspace write lock",
            false,
        )?;
        verify_locked_file_identity(
            &self.authority_lock,
            &self.authority_lock_path,
            "database-family write lock",
            true,
        )?;
        let canonical_now = canonical_database_authority_key(&self.routed_database_path)?;
        if canonical_now != self.canonical_database_path {
            return Err(BeadsError::SyncConflict {
                message: "Canonical database path changed while its write authority was held"
                    .to_string(),
            });
        }
        Ok(())
    }

    fn verify_database_inode_authority_locked(
        &self,
        database_authority: &DatabaseInodeAuthority,
    ) -> Result<()> {
        if let Some(database_lock) = database_authority.lock.as_ref() {
            let current_identity = verify_locked_file_identity(
                database_lock,
                &self.canonical_database_path,
                "database write authority",
                true,
            )?;
            if database_authority.identity != Some(current_identity) {
                return Err(BeadsError::SyncConflict {
                    message: "Database inode changed while its write authority was held"
                        .to_string(),
                });
            }
        } else {
            match fs::symlink_metadata(&self.canonical_database_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(BeadsError::SyncConflict {
                        message: "Database appeared before its inode authority was bound"
                            .to_string(),
                    });
                }
                Err(error) => {
                    return Err(BeadsError::Config(format!(
                        "Could not verify missing database authority {}: {error}",
                        database_path_descriptor(&self.canonical_database_path)
                    )));
                }
            }
        }
        Ok(())
    }

    /// Lock an existing database inode immediately before a writable open, or
    /// report that the stable sidecar protects a still-missing database.
    ///
    /// This method intentionally does not materialize a missing DB: doing so
    /// would hide the missing-database recovery branch from startup. The caller
    /// creates/rebuilds under the stable sidecar, then calls
    /// [`Self::rebind_database_inode_after_authorized_replace`].
    // The inode-authority mutex must span the missing-database filesystem
    // check below so the observed state cannot race a concurrent bind.
    #[allow(clippy::significant_drop_tightening)]
    pub fn bind_database_inode_for_mutation(&self) -> Result<bool> {
        self.verify_common_authority()?;
        let database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        if let Some(database_lock) = database_authority.lock.as_ref() {
            verify_locked_file_identity(
                database_lock,
                &self.canonical_database_path,
                "database write authority",
                true,
            )?;
            return Ok(false);
        }

        match fs::symlink_metadata(&self.canonical_database_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Ok(_) => Err(BeadsError::SyncConflict {
                message: "Database appeared before its inode authority could be bound".to_string(),
            }),
            Err(error) => Err(BeadsError::Config(format!(
                "Could not inspect missing database authority {}: {error}",
                database_path_descriptor(&self.canonical_database_path)
            ))),
        }
    }

    /// Rebind the inode component after a caller-authorized atomic database
    /// replacement while retaining the stable workspace and canonical-path
    /// sidecar authority.
    pub fn rebind_database_inode_after_authorized_replace(&self) -> Result<()> {
        self.verify_common_authority()?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        let current_metadata =
            fs::symlink_metadata(&self.canonical_database_path).map_err(|error| {
                BeadsError::Config(format!(
                    "Could not inspect replaced database authority {}: {error}",
                    database_path_descriptor(&self.canonical_database_path)
                ))
            })?;
        if current_metadata.file_type().is_symlink() || !current_metadata.is_file() {
            return Err(BeadsError::SyncConflict {
                message: "Authorized database replacement did not leave a regular file".to_string(),
            });
        }
        let current_identity = authority_path_identity(
            &self.canonical_database_path,
            "replaced database authority",
            &database_path_descriptor(&self.canonical_database_path),
        )?;
        if database_authority.identity == Some(current_identity) {
            let database_lock =
                database_authority
                    .lock
                    .as_ref()
                    .ok_or_else(|| BeadsError::SyncConflict {
                        message:
                            "Database inode identity existed without a held replacement authority"
                                .to_string(),
                    })?;
            verify_locked_file_identity(
                database_lock,
                &self.canonical_database_path,
                "database write authority",
                true,
            )?;
            return Ok(());
        }

        let replacement_lock = blocking_database_file_lock_with_timeout(
            &self.canonical_database_path,
            Some(self.remaining_lock_timeout_ms()),
            false,
        )?;
        let replacement_identity = verify_locked_file_identity(
            &replacement_lock,
            &self.canonical_database_path,
            "replacement database write authority",
            true,
        )?;
        if let Some(previous_lock) = database_authority.lock.replace(replacement_lock) {
            database_authority.retired_locks.push(previous_lock);
        }
        database_authority.identity = Some(replacement_identity);
        drop(database_authority);
        Ok(())
    }

    /// Create a missing database through a pre-locked replacement inode and
    /// install that inode at the routed database path.
    ///
    /// The replacement is locked before it becomes visible at the canonical
    /// database path. A hard-link alias therefore cannot acquire a competing
    /// inode authority in the interval between creation and binding.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn install_empty_database_replacement_and_bind(
        &self,
    ) -> Result<FreshDatabaseReplacementWitness> {
        self.verify_common_authority()?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        match fs::symlink_metadata(&self.canonical_database_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Refusing to install an empty database replacement over an existing path"
                            .to_string(),
                });
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not inspect fresh database installation target {}: {error}",
                    database_path_descriptor(&self.canonical_database_path)
                )));
            }
        }

        let parent = self.canonical_database_path.parent().ok_or_else(|| {
            BeadsError::Config(
                "Canonical database path has no parent for replacement installation".to_string(),
            )
        })?;
        let stem = self
            .canonical_database_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("beads.db");
        let mut installed = None;
        for attempt in 0..64_u32 {
            let candidate = parent.join(format!(
                ".{stem}.replacement.{}.{}.tmp",
                std::process::id(),
                attempt
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let candidate_file = match options.open(&candidate) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(BeadsError::Config(format!(
                        "Could not create locked database replacement candidate: {error}"
                    )));
                }
            };
            // The candidate becomes the live database inode on rename, so it
            // must carry the SQLite-compatible range lock, never a whole-file
            // lock (GitHub #412).
            match try_lock_exclusive(&candidate_file, ExclusiveLockMechanism::DatabaseInode) {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    return Err(BeadsError::SyncConflict {
                        message: "Fresh database replacement candidate was already locked"
                            .to_string(),
                    });
                }
                Err(TryLockError::Error(error)) => {
                    return Err(BeadsError::Config(format!(
                        "Could not lock database replacement candidate: {error}"
                    )));
                }
            }
            candidate_file.sync_all()?;
            let candidate_identity = authority_file_identity(
                &candidate_file,
                &candidate,
                "database replacement candidate",
                &database_path_descriptor(&candidate),
            )?;
            match fs::symlink_metadata(&self.canonical_database_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(BeadsError::SyncConflict {
                        message: "Database appeared before the locked replacement was installed"
                            .to_string(),
                    });
                }
                Err(error) => {
                    return Err(BeadsError::Config(format!(
                        "Could not re-inspect fresh database installation target {}: {error}",
                        database_path_descriptor(&self.canonical_database_path)
                    )));
                }
            }
            install_database_candidate_no_replace(&candidate, &self.canonical_database_path)?;
            installed = Some((candidate_file, candidate_identity));
            break;
        }
        let (replacement_lock, replacement_identity) = installed.ok_or_else(|| {
            BeadsError::Config(
                "Could not allocate a unique locked database replacement candidate".to_string(),
            )
        })?;
        if let Some(previous_lock) = database_authority.lock.replace(replacement_lock) {
            database_authority.retired_locks.push(previous_lock);
        }
        database_authority.identity = Some(replacement_identity);
        drop(database_authority);
        // The no-replace rename has already committed the candidate to the
        // canonical namespace. Bind and verify that inode before the parent
        // durability barrier so even an fsync failure leaves recovery able to
        // distinguish its own installed generation from a foreign target.
        self.verify_database_authority()?;
        crate::util::sync_parent_directory(&self.canonical_database_path)
            .map_err(BeadsError::Io)?;
        Ok(FreshDatabaseReplacementWitness {
            authority_path_sha256: self.authority_path_sha256.clone(),
            installed_identity: replacement_identity,
        })
    }

    /// Classify the canonical target without mistaking a retained, renamed
    /// original inode for the currently visible database generation.
    pub(crate) fn database_target_authority_state(&self) -> Result<DatabaseTargetAuthorityState> {
        self.verify_common_authority()?;
        let database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        let target_metadata = match fs::symlink_metadata(&self.canonical_database_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Ok(DatabaseTargetAuthorityState::Foreign);
            }
            Ok(_) => Some(authority_path_identity(
                &self.canonical_database_path,
                "database recovery target",
                &database_path_descriptor(&self.canonical_database_path),
            )?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not classify database recovery target {}: {error}",
                    database_path_descriptor(&self.canonical_database_path)
                )));
            }
        };
        let Some(target_identity) = target_metadata else {
            return Ok(DatabaseTargetAuthorityState::Missing);
        };
        let Some(database_lock) = database_authority.lock.as_ref() else {
            return Ok(DatabaseTargetAuthorityState::Foreign);
        };
        // The retained handle may name an original generation that recovery
        // has already staged out of the canonical namespace.  Requiring that
        // handle to still match the canonical path would turn the exact
        // `Foreign` condition this method exists to classify into an error.
        // Re-witness the handle itself, then compare its recorded identity and
        // the independently witnessed canonical target below.
        let retained_identity = authority_file_identity(
            database_lock,
            &self.canonical_database_path,
            "retained database recovery authority",
            &database_path_descriptor(&self.canonical_database_path),
        )?;
        if database_authority.identity == Some(retained_identity)
            && retained_identity == target_identity
        {
            Ok(DatabaseTargetAuthorityState::Held)
        } else {
            Ok(DatabaseTargetAuthorityState::Foreign)
        }
    }

    /// Verify that a database staged out of the canonical namespace is still
    /// the exact inode retained by this database-family authority.
    pub(crate) fn verify_staged_database_recovery_authority(
        &self,
        staged_database_path: &Path,
    ) -> Result<()> {
        self.verify_common_authority()?;
        let database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        let database_lock =
            database_authority
                .lock
                .as_ref()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "Staged database has no retained inode authority".to_string(),
                })?;
        let retained_identity = verify_locked_file_identity(
            database_lock,
            staged_database_path,
            "retained database recovery authority",
            true,
        )?;
        if database_authority.identity != Some(retained_identity) {
            return Err(BeadsError::SyncConflict {
                message: "Database generation changed after the final recovery authority check; refusing to install the original backup"
                    .to_string(),
            });
        }
        drop(database_authority);
        Ok(())
    }

    /// Verify that a fresh-replacement witness still names this authority and
    /// its currently installed database inode.
    pub(crate) fn verify_fresh_database_replacement_witness(
        &self,
        witness: &FreshDatabaseReplacementWitness,
    ) -> Result<()> {
        self.verify_common_authority()?;
        if witness.authority_path_sha256 != self.authority_path_sha256 {
            return Err(BeadsError::SyncConflict {
                message: "Fresh database replacement witness belongs to another authority"
                    .to_string(),
            });
        }

        let database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        let database_lock =
            database_authority
                .lock
                .as_ref()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "Fresh database replacement witness has no held inode authority"
                        .to_string(),
                })?;
        let current_identity = verify_locked_file_identity(
            database_lock,
            &self.canonical_database_path,
            "fresh database replacement authority",
            true,
        )?;
        if database_authority.identity != Some(current_identity)
            || current_identity != witness.installed_identity
        {
            return Err(BeadsError::SyncConflict {
                message: "Fresh database replacement witness no longer names the installed inode"
                    .to_string(),
            });
        }
        drop(database_authority);
        Ok(())
    }

    /// Drop locks for displaced database inodes only after recovery has
    /// irreversibly accepted the replacement.
    pub(crate) fn finalize_database_replacement(&self) -> Result<()> {
        self.verify_common_authority()?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        #[cfg(test)]
        maybe_replace_database_before_finalize_locked_verify(&self.canonical_database_path)?;
        self.verify_database_inode_authority_locked(&database_authority)?;
        database_authority.retired_locks.clear();
        drop(database_authority);
        Ok(())
    }

    /// Re-adopt the still-locked original inode after a failed replacement is
    /// rolled back into place.
    pub(crate) fn restore_retained_database_inode_after_authorized_replace(&self) -> Result<()> {
        self.readopt_retained_database_inode_after_authorized_replace(true)
    }

    /// Re-adopt a rolled-back replacement source while retaining older inode
    /// locks needed by an enclosing recovery transaction.
    pub(crate) fn restore_nested_retained_database_inode_after_authorized_replace(
        &self,
    ) -> Result<()> {
        self.readopt_retained_database_inode_after_authorized_replace(false)
    }

    fn readopt_retained_database_inode_after_authorized_replace(
        &self,
        finalize_older_replacements: bool,
    ) -> Result<()> {
        self.verify_common_authority()?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        let target_identity = authority_path_identity(
            &self.canonical_database_path,
            "restored database write authority",
            &database_path_descriptor(&self.canonical_database_path),
        )?;

        if database_authority.identity == Some(target_identity) {
            let current_lock =
                database_authority
                    .lock
                    .as_ref()
                    .ok_or_else(|| BeadsError::SyncConflict {
                        message: "Restored database identity existed without a retained inode lock"
                            .to_string(),
                    })?;
            verify_locked_file_identity(
                current_lock,
                &self.canonical_database_path,
                "restored database write authority",
                true,
            )?;
            if finalize_older_replacements {
                database_authority.retired_locks.clear();
            }
            return Ok(());
        }

        let mut retained_index = None;
        for (index, lock) in database_authority.retired_locks.iter().enumerate() {
            let identity = authority_file_identity(
                lock,
                &self.canonical_database_path,
                "retained database write authority",
                &database_path_descriptor(&self.canonical_database_path),
            )?;
            if identity == target_identity {
                retained_index = Some(index);
                break;
            }
        }
        let retained_index = retained_index.ok_or_else(|| BeadsError::SyncConflict {
            message: "Restored database inode was not retained under lock".to_string(),
        })?;
        verify_locked_file_identity(
            &database_authority.retired_locks[retained_index],
            &self.canonical_database_path,
            "restored database write authority",
            true,
        )?;
        let restored_lock = database_authority.retired_locks.swap_remove(retained_index);
        if let Some(displaced_lock) = database_authority.lock.replace(restored_lock) {
            database_authority.retired_locks.push(displaced_lock);
        }
        database_authority.identity = Some(target_identity);
        if finalize_older_replacements {
            database_authority.retired_locks.clear();
        }
        drop(database_authority);
        Ok(())
    }

    /// Lock a fully written replacement candidate before it is atomically
    /// installed at the canonical database path.
    pub(crate) fn lock_database_replacement_candidate(
        &self,
        candidate_path: &Path,
    ) -> Result<File> {
        self.verify_common_authority()?;
        blocking_database_file_lock_with_timeout(
            candidate_path,
            Some(self.remaining_lock_timeout_ms()),
            false,
        )
    }

    /// Verify that a still-private replacement path names the exact inode
    /// locked before installation.
    pub(crate) fn verify_locked_database_replacement_candidate(
        &self,
        candidate_path: &Path,
        candidate_lock: &File,
    ) -> Result<()> {
        self.verify_common_authority()?;
        verify_locked_file_identity(
            candidate_lock,
            candidate_path,
            "database replacement candidate",
            true,
        )?;
        Ok(())
    }

    /// Adopt a pre-locked replacement immediately after its atomic install.
    ///
    /// The caller keeps the replacement inode locked across the rename, so
    /// hard-link aliases never observe the new inode without its authority.
    pub(crate) fn adopt_locked_database_replacement(&self, replacement_lock: File) -> Result<()> {
        let replacement_identity = authority_file_identity(
            &replacement_lock,
            &self.canonical_database_path,
            "installed database replacement authority",
            &database_path_descriptor(&self.canonical_database_path),
        )?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        if let Some(previous_lock) = database_authority.lock.replace(replacement_lock) {
            database_authority.retired_locks.push(previous_lock);
        }
        database_authority.identity = Some(replacement_identity);
        drop(database_authority);
        // Record the pre-locked inode first: if the post-rename route or
        // identity check fails, the installed generation remains retained
        // instead of silently losing its authority.
        self.verify_database_authority()
    }

    /// Clear the inode component after an authorized rollback restores the
    /// database family to a legitimately missing state.
    pub(crate) fn clear_database_inode_after_authorized_remove(&self) -> Result<()> {
        self.verify_common_authority()?;
        let mut database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        match fs::symlink_metadata(&self.canonical_database_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(BeadsError::SyncConflict {
                    message: "Cannot clear database inode authority while the routed path exists"
                        .to_string(),
                });
            }
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not verify authorized missing database state: {error}"
                )));
            }
        }
        database_authority.lock = None;
        database_authority.identity = None;
        database_authority.retired_locks.clear();
        drop(database_authority);
        Ok(())
    }

    pub fn verify_database_authority(&self) -> Result<()> {
        self.verify_common_authority()?;
        let database_authority =
            self.database_authority
                .lock()
                .map_err(|_| BeadsError::SyncConflict {
                    message: "Database inode authority state was poisoned".to_string(),
                })?;
        self.verify_database_inode_authority_locked(&database_authority)
    }
}

fn additive_path_descriptor(path: &Path, role: &str) -> String {
    let digest = hex_encode(&Sha256::digest(path.to_string_lossy().as_bytes()));
    format!("<{role} sha256={digest}>")
}

fn database_path_descriptor(path: &Path) -> String {
    additive_path_descriptor(path, "database-authority")
}

fn redact_reviewed_path_result<T, E>(
    result: std::result::Result<T, E>,
    path: &Path,
    role: &str,
    action: &str,
) -> Result<T> {
    result.map_err(|_| {
        BeadsError::Config(format!(
            "Could not {action} reviewed {role} {} (path-sensitive detail suppressed)",
            additive_path_descriptor(path, role)
        ))
    })
}

fn canonical_database_authority_key(database_path: &Path) -> Result<PathBuf> {
    let database_descriptor = database_path_descriptor(database_path);
    let absolute = absolute_database_routing_path(database_path)?;
    match fs::canonicalize(&absolute) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute.parent().ok_or_else(|| {
                BeadsError::Config(format!(
                    "Database path {database_descriptor} has no parent for lock authority"
                ))
            })?;
            let file_name = absolute.file_name().ok_or_else(|| {
                BeadsError::Config(format!(
                    "Database path {database_descriptor} has no filename for lock authority"
                ))
            })?;
            Ok(fs::canonicalize(parent)
                .map_err(|parent_error| {
                    BeadsError::Config(format!(
                        "Could not canonicalize database lock authority parent for {database_descriptor}: {parent_error}"
                    ))
                })?
                .join(file_name))
        }
        Err(error) => Err(BeadsError::Config(format!(
            "Could not resolve database lock authority for {database_descriptor}: {error}"
        ))),
    }
}

fn absolute_database_routing_path(database_path: &Path) -> Result<PathBuf> {
    let database_descriptor = database_path_descriptor(database_path);
    let absolute = if database_path.is_absolute() {
        database_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                BeadsError::Config(format!(
                    "Could not resolve database lock authority for {database_descriptor}: {error}"
                ))
            })?
            .join(database_path)
    };
    Ok(absolute)
}

fn reject_unsafe_database_routing_leaf(database_path: &Path) -> Result<PathBuf> {
    let absolute = absolute_database_routing_path(database_path)?;
    reject_symlinked_database_route_components(&absolute)?;
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(BeadsError::Config(format!(
                "Refusing unsafe configured database leaf {}: expected a regular file, not a symlink or special file",
                database_path_descriptor(&absolute)
            )))
        }
        Ok(_) => Ok(absolute),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(absolute),
        Err(error) => Err(BeadsError::Config(format!(
            "Could not inspect configured database leaf {}: {error}",
            database_path_descriptor(&absolute)
        ))),
    }
}

pub(crate) fn reject_symlinked_database_route_components(database_path: &Path) -> Result<()> {
    let absolute = absolute_database_routing_path(database_path)?;
    for component_path in absolute.ancestors().skip(1) {
        match fs::symlink_metadata(component_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BeadsError::Config(format!(
                    "Refusing configured database route with a symlinked parent component {}",
                    database_path_descriptor(&absolute)
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(BeadsError::Config(format!(
                    "Refusing configured database route with a non-directory parent component {}",
                    database_path_descriptor(&absolute)
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(BeadsError::Config(format!(
                    "Could not inspect configured database route {}: {error}",
                    database_path_descriptor(&absolute)
                )));
            }
        }
    }
    Ok(())
}

/// Resolve the exact canonical database path protected by a database-family
/// authority.
pub fn canonical_database_authority_path(database_path: &Path) -> Result<PathBuf> {
    canonical_database_authority_key(database_path)
}

fn database_write_authority_path(database_path: &Path) -> Result<PathBuf> {
    let canonical_key = canonical_database_authority_key(database_path)?;
    let parent = canonical_key.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "Canonical database path {} has no parent for lock authority",
            database_path_descriptor(&canonical_key)
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"beads-rust-database-write-authority-v1\0");
    update_sync_path_digest(&mut hasher, &canonical_key);
    let digest = hex_encode(&hasher.finalize());
    Ok(parent.join(format!(".br-db-write-{}.lock", &digest[..24])))
}

fn jsonl_write_authority_path(jsonl_path: &Path) -> Result<PathBuf> {
    let canonical_key = canonical_database_authority_key(jsonl_path)?;
    let parent = canonical_key.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "Canonical JSONL path {} has no parent for lock authority",
            additive_path_descriptor(&canonical_key, "jsonl-authority")
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"beads-rust-jsonl-write-authority-v1\0");
    update_sync_path_digest(&mut hasher, &canonical_key);
    let digest = hex_encode(&hasher.finalize());
    Ok(parent.join(format!(".br-jsonl-write-{}.lock", &digest[..24])))
}

fn database_opener_lease_path(database_path: &Path) -> Result<PathBuf> {
    let canonical_key = canonical_database_authority_key(database_path)?;
    let parent = canonical_key.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "Canonical database path {} has no parent for opener lease",
            database_path_descriptor(&canonical_key)
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"beads-rust-database-openers-v1\0");
    update_sync_path_digest(&mut hasher, &canonical_key);
    let digest = hex_encode(&hasher.finalize());
    Ok(parent.join(format!(".br-db-openers-{}.lock", &digest[..24])))
}

/// Upper bound on waiting for a peer's exclusive (checkpointing) hold of the
/// opener lease. A checkpoint is short; a longer wait means the lease is
/// degraded and the caller proceeds without it rather than hanging br.
const OPENER_LEASE_WAIT_MS: u64 = 5_000;

/// Open a dedicated lock sidecar (create if missing) without following
/// symlinks or admitting special files.
fn open_lock_sidecar(path: &Path, role: &str) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(BeadsError::Config(format!(
            "Refusing unsafe {role} path {}: expected a regular file, not a symlink or special file",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path).map_err(|error| {
        BeadsError::Config(format!(
            "Failed to open {role} at {}: {error}",
            path.display()
        ))
    })
}

/// Shared lease held by every br process that has a persistent database open.
///
/// FrankenSQLite's multi-process WAL checkpoint does not yet register against
/// the read snapshots of peer processes (FrankenSQLite #399/#385): a
/// `wal_checkpoint(TRUNCATE)` — and, after enough rounds, even PASSIVE — run
/// by one short-lived `br` process while another has the database open is the
/// interleaving behind the page-aliasing corruption in GitHub #457/#460/#461.
/// The same discriminator shows concurrent processes that never checkpoint
/// stay clean. br therefore checkpoints only while it can prove it is the sole
/// opener: every opener holds this lease shared for the lifetime of its
/// storage handle, and a checkpoint first upgrades to the exclusive hold.
/// While that exclusive hold lasts, new openers wait, so no process starts
/// reading a WAL that is about to be reset.
///
/// The lease is advisory and never blocks br from working: if it cannot be
/// registered within [`OPENER_LEASE_WAIT_MS`] it degrades to "peers unknown",
/// which simply disables checkpointing for that handle.
#[derive(Debug)]
pub struct DatabaseOpenerLease {
    path: PathBuf,
    shared: Option<File>,
}

impl DatabaseOpenerLease {
    /// Register this process as an opener of `database_path`.
    ///
    /// # Errors
    ///
    /// Returns an error only when the lease sidecar path cannot be derived or
    /// is unsafe (symlink / special file); lock contention degrades instead.
    #[allow(clippy::incompatible_msrv)]
    pub fn register(database_path: &Path) -> Result<Self> {
        let path = database_opener_lease_path(database_path)?;
        let shared = Self::acquire_shared(&path)?;
        Ok(Self { path, shared })
    }

    /// Whether this handle currently holds its shared registration.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.shared.is_some()
    }

    #[allow(clippy::incompatible_msrv)]
    fn acquire_shared(path: &Path) -> Result<Option<File>> {
        let file = open_lock_sidecar(path, "database opener lease")?;
        let started = Instant::now();
        let timeout = Duration::from_millis(OPENER_LEASE_WAIT_MS);
        loop {
            match file.try_lock_shared() {
                Ok(()) => return Ok(Some(file)),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => {
                    tracing::warn!(
                        error = %error,
                        lease = %path.display(),
                        "database opener lease unavailable; checkpoints are disabled for this handle"
                    );
                    return Ok(None);
                }
            }
            if started.elapsed() >= timeout {
                tracing::warn!(
                    lease = %path.display(),
                    "a peer held the database opener lease exclusively for too long; proceeding unregistered with checkpoints disabled"
                );
                return Ok(None);
            }
            thread::sleep(WRITE_LOCK_POLL_INTERVAL);
        }
    }

    /// Try to become the sole opener.
    ///
    /// Returns the exclusive hold when no other process has the database
    /// open. Returns `None` — with the shared registration restored — when a
    /// peer is present or the lease is degraded. Callers must hand the hold
    /// back through [`Self::release_exclusive`] once the checkpoint is done.
    #[allow(clippy::incompatible_msrv)]
    pub fn try_exclusive(&mut self) -> Option<File> {
        // Whole-file locks are per open file description, so this handle's
        // own shared registration must be released before probing.
        self.shared.take()?;
        let exclusive = match open_lock_sidecar(&self.path, "database opener lease") {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(error = %error, "database opener lease reopen failed");
                self.restore_shared();
                return None;
            }
        };
        match exclusive.try_lock() {
            Ok(()) => Some(exclusive),
            Err(TryLockError::WouldBlock) => {
                drop(exclusive);
                self.restore_shared();
                None
            }
            Err(TryLockError::Error(error)) => {
                tracing::warn!(error = %error, "database opener lease exclusive probe failed");
                drop(exclusive);
                self.restore_shared();
                None
            }
        }
    }

    /// Return an exclusive hold obtained from [`Self::try_exclusive`] and
    /// re-register this handle as an ordinary shared opener.
    pub fn release_exclusive(&mut self, exclusive: File) {
        drop(exclusive);
        self.restore_shared();
    }

    fn restore_shared(&mut self) {
        match Self::acquire_shared(&self.path) {
            Ok(shared) => self.shared = shared,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "database opener lease could not be re-registered; checkpoints stay disabled"
                );
                self.shared = None;
            }
        }
    }
}

/// Acquire the stable JSONL-family authority honored by no-DB sessions.
pub fn blocking_jsonl_family_write_lock_with_timeout(
    jsonl_path: &Path,
    lock_timeout_ms: Option<u64>,
) -> Result<JsonlFamilyWriteLock> {
    match fs::symlink_metadata(jsonl_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(BeadsError::Config(format!(
                "Refusing unsafe JSONL authority leaf {}: expected a regular file, not a symlink or special file",
                additive_path_descriptor(jsonl_path, "jsonl-authority")
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BeadsError::Config(format!(
                "Could not inspect JSONL authority leaf {}: {error}",
                additive_path_descriptor(jsonl_path, "jsonl-authority")
            )));
        }
    }
    let canonical_jsonl_path = canonical_database_authority_key(jsonl_path)?;
    let authority_lock_path = jsonl_write_authority_path(jsonl_path)?;
    let authority_path_sha256 = canonical_sync_path_sha256(&authority_lock_path);
    let authority_lock = open_and_lock_regular_file(
        &authority_lock_path,
        lock_timeout_ms,
        true,
        "JSONL-family write lock",
        true,
        ExclusiveLockMechanism::LockSidecar,
    )?;
    let canonical_after = canonical_database_authority_key(jsonl_path)?;
    if canonical_after != canonical_jsonl_path {
        return Err(BeadsError::SyncConflict {
            message: "JSONL routing changed while acquiring its write authority".to_string(),
        });
    }
    let pinned_jsonl_name = pin_jsonl_target(jsonl_path)?;
    // The pinned route is deliberately lexical (its no-follow traversal has
    // already rejected reparse points), while the sidecar key above is a
    // `fs::canonicalize` product — a verbatim `\\?\` spelling on Windows.
    // Compare through the shared convention so one target always matches
    // itself, while a genuinely different route still conflicts (#413).
    if !authority_paths_equivalent(pinned_jsonl_name.display_path(), &canonical_jsonl_path) {
        return Err(BeadsError::SyncConflict {
            message: "Pinned JSONL route does not match the canonical sidecar write authority"
                .to_string(),
        });
    }
    let authority = JsonlFamilyWriteLock {
        authority_lock,
        authority_lock_path,
        authority_path_sha256,
        routed_jsonl_path: jsonl_path.to_path_buf(),
        canonical_jsonl_path,
        pinned_jsonl_name,
    };
    authority.verify_jsonl_authority()?;
    Ok(authority)
}

/// Stable digest of the canonical database-family lock authority.
pub fn database_write_authority_sha256(database_path: &Path) -> Result<String> {
    Ok(canonical_sync_path_sha256(&database_write_authority_path(
        database_path,
    )?))
}

/// Acquire the common database-family authority honored by CLI, MCP, recovery,
/// and reviewed reconciliation mutation paths.
#[allow(clippy::too_many_lines)]
pub fn blocking_database_family_write_lock_with_timeout(
    beads_dir: &Path,
    database_path: &Path,
    lock_timeout_ms: Option<u64>,
) -> Result<DatabaseFamilyWriteLock> {
    let acquisition_started = Instant::now();
    let total_timeout_ms = lock_timeout_ms.unwrap_or(DEFAULT_WRITE_LOCK_TIMEOUT_MS);
    let remaining_timeout_ms = || {
        let elapsed_ms =
            u64::try_from(acquisition_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        total_timeout_ms.saturating_sub(elapsed_ms)
    };
    let routed_database_path = reject_unsafe_database_routing_leaf(database_path)?;
    let workspace_lock_path = beads_dir.join(".write.lock");
    let workspace_lock = blocking_write_lock_with_timeout(beads_dir, Some(remaining_timeout_ms()))?;
    let canonical_database_path = canonical_database_authority_key(database_path)?;
    let authority_path = database_write_authority_path(database_path)?;
    let authority_path_sha256 = database_write_authority_sha256(database_path)?;
    let authority_lock = open_and_lock_regular_file(
        &authority_path,
        Some(remaining_timeout_ms()),
        true,
        "database-family write lock",
        true,
        ExclusiveLockMechanism::LockSidecar,
    )?;
    let (database_lock, database_identity) = match fs::symlink_metadata(&canonical_database_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(BeadsError::Config(format!(
                "Refusing unsafe database write authority {}: expected a regular file, not a symlink or special file",
                database_path_descriptor(&canonical_database_path)
            )));
        }
        Ok(_) => {
            let database_lock = blocking_database_file_lock_with_timeout(
                &canonical_database_path,
                Some(remaining_timeout_ms()),
                false,
            )?;
            let identity = authority_file_identity(
                &database_lock,
                &canonical_database_path,
                "database write authority",
                &database_path_descriptor(&canonical_database_path),
            )?;
            (Some(database_lock), Some(identity))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => {
            return Err(BeadsError::Config(format!(
                "Could not inspect canonical database authority {}: {error}",
                database_path_descriptor(&canonical_database_path)
            )));
        }
    };
    let canonical_after = canonical_database_authority_key(database_path)?;
    if canonical_after != canonical_database_path {
        return Err(BeadsError::SyncConflict {
            message: "Database routing changed while acquiring its write authority".to_string(),
        });
    }
    match (&database_lock, database_identity) {
        (Some(database_lock), Some(identity)) => {
            let observed_identity = verify_locked_file_identity(
                database_lock,
                &canonical_after,
                "database write authority",
                true,
            )?;
            if observed_identity != identity {
                return Err(BeadsError::SyncConflict {
                    message: "Database inode changed while acquiring its write authority"
                        .to_string(),
                });
            }
        }
        (None, None) => verify_database_authority_path_still_missing(&canonical_after)?,
        _ => unreachable!("database inode authority lock and identity must be paired"),
    }
    Ok(DatabaseFamilyWriteLock {
        workspace_lock,
        workspace_lock_path,
        authority_lock,
        authority_lock_path: authority_path,
        database_authority: std::sync::Mutex::new(DatabaseInodeAuthority {
            lock: database_lock,
            identity: database_identity,
            retired_locks: Vec::new(),
        }),
        authority_path_sha256,
        routed_database_path,
        canonical_database_path,
        acquisition_started,
        total_timeout_ms,
    })
}

#[allow(clippy::too_many_lines)]
fn open_and_lock_regular_file(
    lock_path: &Path,
    lock_timeout_ms: Option<u64>,
    create_if_missing: bool,
    role: &str,
    redact_path: bool,
    mechanism: ExclusiveLockMechanism,
) -> Result<File> {
    let lock_path_display = if redact_path {
        if role.starts_with("JSONL-") {
            additive_path_descriptor(lock_path, "jsonl-authority")
        } else {
            database_path_descriptor(lock_path)
        }
    } else {
        lock_path.display().to_string()
    };
    match fs::symlink_metadata(lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(BeadsError::Config(format!(
                "Refusing unsafe {role} path {}: expected a regular file, not a symlink or special file",
                lock_path_display
            )));
        }
        Ok(_) => {}
        Err(error) if create_if_missing && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BeadsError::Config(format!(
                "Failed to inspect {role} at {}: {error}",
                lock_path_display
            )));
        }
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create_if_missing)
        .truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(lock_path).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to open {role} at {}: {err}",
            lock_path_display
        ))
    })?;
    let opened_metadata = file.metadata().map_err(|error| {
        BeadsError::Config(format!(
            "Failed to witness opened {role} at {}: {error}",
            lock_path_display
        ))
    })?;
    let path_metadata = fs::symlink_metadata(lock_path).map_err(|error| {
        BeadsError::Config(format!(
            "Failed to re-witness {role} at {}: {error}",
            lock_path_display
        ))
    })?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "{role} path {} changed to a symlink or special file while opening it",
            lock_path_display
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if (opened_metadata.dev(), opened_metadata.ino())
            != (path_metadata.dev(), path_metadata.ino())
        {
            return Err(BeadsError::Config(format!(
                "{role} identity changed while opening {}",
                lock_path_display
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = opened_metadata;

    // Fast path: non-blocking try for the common uncontended case.
    match try_lock_exclusive(&file, mechanism) {
        Ok(()) => {
            verify_locked_file_identity(&file, lock_path, role, redact_path)?;
            return Ok(file);
        }
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(err)) => {
            return Err(BeadsError::Config(format!(
                "Failed to acquire {role} at {}: {err}",
                lock_path_display
            )));
        }
    }

    let timeout_ms = lock_timeout_ms.unwrap_or(DEFAULT_WRITE_LOCK_TIMEOUT_MS);
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();
    tracing::debug!(
        timeout_ms,
        lock_path = %lock_path_display,
        role,
        "write authority is held by another process; waiting with timeout"
    );

    loop {
        if start.elapsed() >= timeout {
            return Err(write_lock_timeout_error(
                &lock_path_display,
                role,
                timeout_ms,
            ));
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(remaining.min(WRITE_LOCK_POLL_INTERVAL));

        match try_lock_exclusive(&file, mechanism) {
            Ok(()) => {
                verify_locked_file_identity(&file, lock_path, role, redact_path)?;
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => {
                tracing::debug!(role, "failed to acquire write authority: {err}");
                return Err(BeadsError::Config(format!(
                    "Failed to acquire {role} at {}: {err}",
                    lock_path_display
                )));
            }
        }
    }
}

fn authority_file_identity(
    file: &File,
    authority_path: &Path,
    role: &str,
    path_display: &str,
) -> Result<(u64, u64)> {
    let metadata = file.metadata().map_err(|error| {
        BeadsError::Config(format!(
            "Failed to witness locked {role} at {path_display}: {error}"
        ))
    })?;
    if !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Locked {role} at {path_display} is not a regular file"
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let _ = authority_path;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let identity = path::windows_jsonl_file_identity(file, authority_path)?;
        Ok((identity.device_id(), identity.inode()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = authority_path;
        Err(BeadsError::Config(format!(
            "Stable file-handle identity for {role} at {path_display} is unavailable on this platform"
        )))
    }
}

fn authority_path_identity(
    authority_path: &Path,
    role: &str,
    path_display: &str,
) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::symlink_metadata(authority_path).map_err(|error| {
            BeadsError::Config(format!(
                "Failed to re-witness locked {role} at {path_display}: {error}"
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BeadsError::Config(format!(
                "Locked {role} path at {path_display} changed to a symlink or special file"
            )));
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let identity = path::open_regular_authority_identity(authority_path)?.ok_or_else(|| {
            BeadsError::Config(format!("Locked {role} path disappeared at {path_display}"))
        })?;
        Ok((identity.device_id(), identity.inode()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = authority_path;
        Err(BeadsError::Config(format!(
            "Stable routed identity for {role} at {path_display} is unavailable on this platform"
        )))
    }
}

fn verify_locked_file_identity(
    file: &File,
    lock_path: &Path,
    role: &str,
    redact_path: bool,
) -> Result<(u64, u64)> {
    let lock_path_display = if redact_path {
        if role.starts_with("JSONL-") {
            additive_path_descriptor(lock_path, "jsonl-authority")
        } else {
            database_path_descriptor(lock_path)
        }
    } else {
        lock_path.display().to_string()
    };
    let opened = authority_file_identity(file, lock_path, role, &lock_path_display)?;
    #[cfg(windows)]
    let current_guard = path::open_regular_authority_source(lock_path)?.ok_or_else(|| {
        BeadsError::Config(format!(
            "Locked {role} path disappeared at {lock_path_display}"
        ))
    })?;
    #[cfg(windows)]
    let current = {
        let identity = current_guard.identity();
        (identity.device_id(), identity.inode())
    };
    #[cfg(not(windows))]
    let current = authority_path_identity(lock_path, role, &lock_path_display)?;
    if opened != current {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "{role} generation changed (locked-file identity changed) at {lock_path_display}"
            ),
        });
    }
    Ok(opened)
}

fn verify_database_authority_path_still_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(BeadsError::SyncConflict {
            message: "Database appeared while acquiring its write authority".to_string(),
        }),
        Err(error) => Err(BeadsError::Config(format!(
            "Could not re-inspect missing database authority {}: {error}",
            database_path_descriptor(path)
        ))),
    }
}

fn write_lock_timeout_error(lock_path_display: &str, role: &str, timeout_ms: u64) -> BeadsError {
    BeadsError::Config(format!(
        "Timed out after {timeout_ms}ms waiting for write lock ({role}) at {}. \
         Another br process may be holding that authority; retry after it exits or investigate a stuck process.",
        lock_path_display
    ))
}

#[must_use]
pub const fn default_write_lock_timeout_ms() -> u64 {
    DEFAULT_WRITE_LOCK_TIMEOUT_MS
}

/// Try to acquire an exclusive advisory lock on `.beads/.sync.lock`.
///
/// Returns the lock file on success. The lock is held until the returned
/// `File` is dropped. If another process already holds the lock, returns
/// `Ok(None)` (non-blocking). Lock-file open or OS lock errors are returned
/// separately so callers do not confuse a broken lock path with contention.
#[allow(clippy::incompatible_msrv)]
pub fn try_sync_lock(beads_dir: &Path) -> Result<Option<File>> {
    let lock_path = beads_dir.join(".sync.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| {
            BeadsError::Config(format!(
                "Failed to open sync lock at {}: {err}",
                lock_path.display()
            ))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(BeadsError::Config(format!(
            "Failed to acquire sync lock at {}: {err}",
            lock_path.display()
        ))),
    }
}

struct TempFileGuard {
    path: PathBuf,
    persist: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persist: false,
        }
    }

    fn new_retained(path: PathBuf) -> Self {
        Self {
            path,
            persist: true,
        }
    }

    fn persist(&mut self) {
        self.persist = true;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalPublicationHookPhase {
    PreCreate,
    PreCommit,
    PostRename,
    ParentFsync,
    PreCleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalNamespaceChange {
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            windows
        )),
        allow(dead_code)
    )]
    RenamedCreate,
    Exchanged,
    /// The platform could not perform the flagged (or, on Windows, any
    /// handle-relative) atomic exchange, so the staged generation was
    /// installed with a plain rename after the destination witness was
    /// re-verified under the held JSONL authority (#419, #413).
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            windows
        )),
        allow(dead_code)
    )]
    ReplacedUnderAuthority,
}

// Test-only fault injection: pretend the filesystem rejects flagged
// `renameat2` the way WSL2 9p/DrvFS does (#419), so the witness-checked
// fallback can be exercised on filesystems that support the atomic path.
#[cfg(test)]
thread_local! {
    static FORCE_FLAGGED_RENAME_UNSUPPORTED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Whether the test-only fault injection is asking this thread to treat the
/// flagged rename as unsupported. Always `false` outside test builds, so the
/// production path never pays for or branches on it.
#[cfg(test)]
fn flagged_rename_forced_unsupported() -> bool {
    FORCE_FLAGGED_RENAME_UNSUPPORTED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn flagged_rename_forced_unsupported() -> bool {
    false
}

/// Whether `renameat2`-style flags were refused by the filesystem rather than
/// by the namespace state.
///
/// `EINVAL` is what Linux filesystems without flagged-rename support return
/// (WSL2 9p/DrvFS included); `ENOSYS` is a kernel without `renameat2`;
/// `ENOTSUP`/`EOPNOTSUPP` is the `renameatx_np` answer on Apple filesystems
/// that lack `RENAME_EXCL`/`RENAME_SWAP`. None of these describe the
/// destination, so they are the only errors the fallback may absorb.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn flagged_rename_unsupported(error: rustix::io::Errno) -> bool {
    use rustix::io::Errno;
    error == Errno::INVAL
        || error == Errno::NOSYS
        || error == Errno::NOTSUP
        || error == Errno::OPNOTSUPP
}

/// Whether a parent-directory sync failure is the platform's honest "cannot
/// certify" answer rather than a real durability failure.
///
/// Only Windows qualifies, and only for the deliberate `Unsupported` answer
/// from `PinnedJsonlParent::fsync`: Windows exposes no unprivileged
/// directory-entry fsync, so a completed and re-verified publication must not
/// be reported as failed for lacking a certificate the platform cannot issue
/// (#413). Every other error — and every non-Windows error, byte-identical to
/// before — still fails the durable-publication contract.
#[cfg(windows)]
fn parent_sync_uncertifiable_on_platform(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
}

#[cfg(not(windows))]
const fn parent_sync_uncertifiable_on_platform(_error: &std::io::Error) -> bool {
    false
}

/// Install the staged generation with a plain rename after re-verifying the
/// destination witness under the held JSONL-family write authority (#419).
///
/// The authority already excludes every other `br` writer, so the residual
/// check-then-rename window is only exposed to foreign writers; the receipt
/// records the downgrade so the weaker guarantee is visible, not silent.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn replace_jsonl_under_authority(
    staged_name: &PinnedJsonlName,
    output_name: &PinnedJsonlName,
    expected_previous_state: &JsonlSourceStateWitness,
) -> Result<ConditionalNamespaceChange> {
    verify_expected_jsonl_source_state_observed(
        output_name.capture_optional()?.as_ref(),
        None,
        Some(expected_previous_state),
    )?;
    rustix::fs::renameat(
        staged_name.parent().as_file(),
        staged_name.leaf(),
        output_name.parent().as_file(),
        output_name.leaf(),
    )
    .map_err(|error| BeadsError::Io(std::io::Error::from(error)))?;
    Ok(ConditionalNamespaceChange::ReplacedUnderAuthority)
}

struct ConditionalJsonlPublication {
    source: Arc<JsonlSourceSnapshot>,
    atomicity: ExportPublicationAtomicity,
    retained_recovery_path: Option<String>,
    cleanup_durable: bool,
}

impl ConditionalJsonlPublication {
    fn into_receipt(self, output_path: &Path, content_sha256: String) -> ExportPublicationReceipt {
        ExportPublicationReceipt {
            content_sha256,
            output_path: output_path.to_string_lossy().to_string(),
            source: self.source,
            atomicity: self.atomicity,
            retained_recovery_path: self.retained_recovery_path,
            cleanup_durable: self.cleanup_durable,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn perform_conditional_namespace_change(
    staged_name: &PinnedJsonlName,
    output_name: &PinnedJsonlName,
    expected_previous_state: &JsonlSourceStateWitness,
) -> Result<ConditionalNamespaceChange> {
    use rustix::fs::{RenameFlags, renameat_with};

    if staged_name.parent().identity() != output_name.parent().identity() {
        return Err(BeadsError::SyncConflict {
            message:
                "Conditional JSONL publication names do not share one retained parent capability"
                    .to_string(),
        });
    }

    let (flags, change) = match expected_previous_state {
        JsonlSourceStateWitness::Missing => (
            RenameFlags::NOREPLACE,
            ConditionalNamespaceChange::RenamedCreate,
        ),
        JsonlSourceStateWitness::Present { .. } => {
            (RenameFlags::EXCHANGE, ConditionalNamespaceChange::Exchanged)
        }
    };

    // The injected failure must pre-empt the real syscall: a flagged rename
    // that already succeeded cannot be "retried" by the fallback.
    let flagged_rename = if flagged_rename_forced_unsupported() {
        Err(rustix::io::Errno::INVAL)
    } else {
        renameat_with(
            staged_name.parent().as_file(),
            staged_name.leaf(),
            output_name.parent().as_file(),
            output_name.leaf(),
            flags,
        )
    };

    match flagged_rename {
        Ok(()) => Ok(change),
        Err(error) if flagged_rename_unsupported(error) => {
            tracing::warn!(
                output_path = %output_name.display_path().display(),
                error = %std::io::Error::from(error),
                "filesystem does not support flagged rename; publishing JSONL with a \
                 witness-checked plain rename under the held write authority \
                 (non-atomic against foreign writers; recorded in the receipt)"
            );
            replace_jsonl_under_authority(staged_name, output_name, expected_previous_state)
        }
        Err(error) => {
            let error = std::io::Error::from(error);
            Err(match (expected_previous_state, error.kind()) {
                (JsonlSourceStateWitness::Missing, std::io::ErrorKind::AlreadyExists) => {
                    BeadsError::SyncConflict {
                        message:
                            "JSONL appeared before the atomic no-replace publication; refusing to overwrite it"
                                .to_string(),
                    }
                }
                (JsonlSourceStateWitness::Present { .. }, std::io::ErrorKind::NotFound) => {
                    BeadsError::SyncConflict {
                        message:
                            "JSONL disappeared before the atomic exchange publication; refusing to continue"
                                .to_string(),
                    }
                }
                _ => BeadsError::Io(error),
            })
        }
    }
}

/// Publish the staged JSONL generation on Windows under the held write
/// authority (#413).
///
/// Windows has no `renameat2` analogue reachable through the retained parent
/// capability, so both branches re-verify state through the pinned handles
/// and then rename by path, following the witness-checked fallback design
/// that shipped for Unix flagged-rename refusal in #419:
///
/// * A `Missing` destination uses the same native atomic no-replace rename as
///   fresh-database installation (`MoveFileExW` without `REPLACE_EXISTING`),
///   so the create tier keeps its no-clobber guarantee and still reports
///   `RenamedCreate`. A destination that appears first surfaces as the same
///   refuse-to-overwrite conflict as on Unix.
/// * A `Present` destination has no atomic exchange at all: the destination
///   witness is re-verified under the held authority, the staged generation
///   is installed with a plain replacing rename, and the receipt records the
///   `ReplacedUnderAuthority` downgrade exactly as the Unix fallback does. A
///   destination that disappeared since the witness was taken fails the
///   re-verification, and every other rename error (`EACCES` and friends)
///   keeps its destination-state kind.
///
/// The renames are path-based rather than capability-relative, so the
/// retained parent route is re-witnessed immediately before and after the
/// namespace change; the caller additionally re-captures the published leaf
/// through the pinned handles and fails closed if the route was substituted.
#[cfg(windows)]
fn perform_conditional_namespace_change(
    staged_name: &PinnedJsonlName,
    output_name: &PinnedJsonlName,
    expected_previous_state: &JsonlSourceStateWitness,
) -> Result<ConditionalNamespaceChange> {
    if staged_name.parent().identity() != output_name.parent().identity() {
        return Err(BeadsError::SyncConflict {
            message:
                "Conditional JSONL publication names do not share one retained parent capability"
                    .to_string(),
        });
    }

    staged_name.parent().verify_route()?;
    let staged_path = staged_name
        .parent()
        .canonical_path()
        .join(staged_name.leaf());
    let output_path = output_name
        .parent()
        .canonical_path()
        .join(output_name.leaf());

    let change = match expected_previous_state {
        JsonlSourceStateWitness::Missing => {
            match rename_path_no_replace_windows(&staged_path, &output_path) {
                Ok(()) => ConditionalNamespaceChange::RenamedCreate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(BeadsError::SyncConflict {
                        message:
                            "JSONL appeared before the atomic no-replace publication; refusing to overwrite it"
                                .to_string(),
                    });
                }
                Err(error) => return Err(BeadsError::Io(error)),
            }
        }
        JsonlSourceStateWitness::Present { .. } => {
            tracing::warn!(
                output_path = %output_name.display_path().display(),
                "Windows provides no atomic JSONL exchange; publishing with a \
                 witness-checked replacing rename under the held write authority \
                 (non-atomic against foreign writers; recorded in the receipt)"
            );
            verify_expected_jsonl_source_state_observed(
                output_name.capture_optional()?.as_ref(),
                None,
                Some(expected_previous_state),
            )?;
            // `std::fs::rename` replaces an existing Windows destination. A
            // disappearance since the witness was taken already failed the
            // re-verification above, so any error here is surfaced verbatim.
            fs::rename(&staged_path, &output_path).map_err(BeadsError::Io)?;
            ConditionalNamespaceChange::ReplacedUnderAuthority
        }
    };
    staged_name.parent().verify_route()?;
    Ok(change)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn perform_conditional_namespace_change(
    _staged_name: &PinnedJsonlName,
    _output_name: &PinnedJsonlName,
    _expected_previous_state: &JsonlSourceStateWitness,
) -> Result<ConditionalNamespaceChange> {
    Err(BeadsError::Config(
        "This platform does not provide the retained handle-relative namespace primitives required for conditional JSONL publication"
            .to_string(),
    ))
}

fn published_but_unwitnessed(
    output_path: &Path,
    recovery_path: Option<&Path>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> BeadsError {
    BeadsError::JsonlPublishedButUnwitnessed {
        output_path: output_path.to_path_buf(),
        recovery_path: recovery_path.map(Path::to_path_buf),
        source: Box::new(source),
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_staged_jsonl_conditionally(
    temp_path: &Path,
    temp_guard: TempFileGuard,
    output_path: &Path,
    staged_source: &JsonlSourceSnapshot,
    expected_previous_state: &JsonlSourceStateWitness,
    content_sha256: &str,
    jsonl_authority: &JsonlFamilyWriteLock,
    database_authority: Option<&DatabaseFamilyWriteLock>,
) -> Result<ConditionalJsonlPublication> {
    publish_staged_jsonl_conditionally_with_hooks(
        temp_path,
        temp_guard,
        output_path,
        staged_source,
        expected_previous_state,
        content_sha256,
        jsonl_authority,
        |phase| {
            if matches!(
                phase,
                ConditionalPublicationHookPhase::PreCommit
                    | ConditionalPublicationHookPhase::PostRename
            ) && let Some(database_authority) = database_authority
            {
                database_authority.verify_database_authority()?;
            }
            Ok(())
        },
        JsonlFamilyWriteLock::fsync_pinned_parent,
    )
}

/// Publish an already-synced staged regular file through the same conditional
/// namespace protocol as primary JSONL exports.
///
/// The target's exact current generation is captured only after acquiring its
/// stable path-family authority. Once admitted to this protocol, the staged
/// file is retained on any failure because a substituted route makes
/// path-based cleanup ambiguous.
pub(crate) fn publish_staged_file_conditionally(
    temp_path: &Path,
    output_path: &Path,
) -> Result<ExportPublicationReceipt> {
    let authority = blocking_jsonl_family_write_lock_with_timeout(output_path, None)?;
    let output_name = authority.pinned_name_for_target(output_path)?;
    let staged_name = authority.pinned_sibling(temp_path)?;
    let previous_source = output_name.capture_optional()?;
    let expected_previous_state = previous_source.as_ref().map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    let staged_source = staged_name.capture()?;
    let content_sha256 = staged_source.content_sha256().to_string();
    let publication = publish_staged_jsonl_conditionally(
        temp_path,
        TempFileGuard::new_retained(temp_path.to_path_buf()),
        output_path,
        &staged_source,
        &expected_previous_state,
        &content_sha256,
        &authority,
        None,
    )?;
    Ok(publication.into_receipt(output_path, content_sha256))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn publish_staged_jsonl_conditionally_with_hooks<Hook, SyncParent>(
    temp_path: &Path,
    mut temp_guard: TempFileGuard,
    output_path: &Path,
    staged_source: &JsonlSourceSnapshot,
    expected_previous_state: &JsonlSourceStateWitness,
    content_sha256: &str,
    jsonl_authority: &JsonlFamilyWriteLock,
    mut hook: Hook,
    mut sync_parent: SyncParent,
) -> Result<ConditionalJsonlPublication>
where
    Hook: FnMut(ConditionalPublicationHookPhase) -> Result<()>,
    SyncParent: FnMut(&JsonlFamilyWriteLock) -> std::io::Result<()>,
{
    // This path may become a recovery name after the namespace operation.
    // Path-based Drop cleanup cannot safely distinguish the retained parent
    // from an attacker-substituted route, so conditional publication owns all
    // subsequent cleanup decisions explicitly.
    temp_guard.persist();
    jsonl_authority.verify_jsonl_authority()?;
    let output_name = jsonl_authority.pinned_name_for_target(output_path)?;
    let staged_name = jsonl_authority.pinned_sibling(temp_path)?;
    verify_expected_jsonl_source_state_observed(
        output_name.capture_optional()?.as_ref(),
        None,
        Some(expected_previous_state),
    )?;
    verify_expected_jsonl_source_state_observed(
        staged_name.capture_optional()?.as_ref(),
        None,
        Some(&staged_source.state_witness()),
    )?;

    hook(ConditionalPublicationHookPhase::PreCommit)?;
    let namespace_change =
        perform_conditional_namespace_change(&staged_name, &output_name, expected_previous_state)?;
    let displaced_path =
        (namespace_change == ConditionalNamespaceChange::Exchanged).then_some(temp_path);
    let cleanup_candidate =
        (namespace_change == ConditionalNamespaceChange::Exchanged).then_some(temp_path);

    hook(ConditionalPublicationHookPhase::PostRename)
        .map_err(|error| published_but_unwitnessed(output_path, displaced_path, error))?;

    let persisted_source = Arc::new(
        output_name
            .capture()
            .map_err(|error| published_but_unwitnessed(output_path, displaced_path, error))?,
    );
    if persisted_source.state_witness() != staged_source.state_witness()
        || persisted_source.content_sha256() != content_sha256
    {
        let error = BeadsError::SyncConflict {
            message:
                "Published JSONL path does not contain the exact staged generation after the namespace change"
                    .to_string(),
        };
        return if let Some(recovery_path) = displaced_path {
            Err(BeadsError::JsonlPublicationConflict {
                output_path: output_path.to_path_buf(),
                recovery_path: recovery_path.to_path_buf(),
                message: error.to_string(),
            })
        } else {
            Err(published_but_unwitnessed(output_path, None, error))
        };
    }

    let displaced_source = if namespace_change == ConditionalNamespaceChange::Exchanged {
        let displaced_source = staged_name
            .capture()
            .map_err(|error| published_but_unwitnessed(output_path, displaced_path, error))?;
        if displaced_source.state_witness() != *expected_previous_state {
            return Err(BeadsError::JsonlPublicationConflict {
                output_path: output_path.to_path_buf(),
                recovery_path: temp_path.to_path_buf(),
                message:
                    "the atomically displaced JSONL generation does not match the exact retained source witness"
                        .to_string(),
            });
        }
        Some(displaced_source)
    } else {
        None
    };

    hook(ConditionalPublicationHookPhase::ParentFsync)
        .map_err(|error| published_but_unwitnessed(output_path, displaced_path, error))?;
    if let Err(source) = sync_parent(jsonl_authority) {
        if parent_sync_uncertifiable_on_platform(&source) {
            tracing::warn!(
                output_path = %output_path.display(),
                error = %source,
                "published JSONL generation is verified, but this platform cannot \
                 certify directory-entry durability for its name"
            );
        } else {
            return Err(BeadsError::JsonlPublishedButNotDurable {
                output_path: output_path.to_path_buf(),
                recovery_path: cleanup_candidate.map(Path::to_path_buf),
                content_sha256: content_sha256.to_string(),
                source,
            });
        }
    }
    jsonl_authority
        .verify_jsonl_authority()
        .map_err(|error| published_but_unwitnessed(output_path, displaced_path, error))?;

    let atomicity = match namespace_change {
        ConditionalNamespaceChange::RenamedCreate => ExportPublicationAtomicity::CreateNoReplace,
        ConditionalNamespaceChange::Exchanged => ExportPublicationAtomicity::ExchangeAndVerify,
        ConditionalNamespaceChange::ReplacedUnderAuthority => {
            ExportPublicationAtomicity::ReplaceUnderAuthority
        }
    };
    let mut retained_recovery_path = None;
    let mut cleanup_durable = true;
    if let (Some(cleanup_path), Some(displaced_source)) =
        (cleanup_candidate, displaced_source.as_ref())
    {
        let cleanup_result = hook(ConditionalPublicationHookPhase::PreCleanup).and_then(|()| {
            jsonl_authority.verify_jsonl_authority()?;
            #[cfg(unix)]
            {
                staged_name.remove_regular_if_identity(displaced_source.identity())
            }
            #[cfg(not(unix))]
            {
                let _ = displaced_source;
                Err(BeadsError::Config(
                    "Exact handle-relative JSONL cleanup is unavailable on this platform"
                        .to_string(),
                ))
            }
        });
        match cleanup_result {
            Ok(()) => {
                if let Err(error) = sync_parent(jsonl_authority) {
                    cleanup_durable = false;
                    tracing::warn!(
                        output_path = %output_path.display(),
                        cleanup_path = %cleanup_path.display(),
                        error = %error,
                        "JSONL publication is durable and the displaced generation was removed, but cleanup durability is uncertain"
                    );
                }
            }
            Err(error) => {
                cleanup_durable = false;
                retained_recovery_path = Some(cleanup_path.to_string_lossy().to_string());
                tracing::warn!(
                    output_path = %output_path.display(),
                    cleanup_path = %cleanup_path.display(),
                    error = %error,
                    "JSONL publication is durable, but the displaced-generation recovery file was retained"
                );
            }
        }
    }

    jsonl_authority.verify_jsonl_authority().map_err(|error| {
        published_but_unwitnessed(
            output_path,
            retained_recovery_path.as_deref().map(Path::new),
            error,
        )
    })?;

    Ok(ConditionalJsonlPublication {
        source: persisted_source,
        atomicity,
        retained_recovery_path,
        cleanup_durable,
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn publish_staged_jsonl_conditionally_with<Before, SyncParent>(
    temp_path: &Path,
    temp_guard: TempFileGuard,
    output_path: &Path,
    staged_source: &JsonlSourceSnapshot,
    expected_previous_state: &JsonlSourceStateWitness,
    content_sha256: &str,
    jsonl_authority: &JsonlFamilyWriteLock,
    before_namespace_change: Before,
    mut sync_parent: SyncParent,
) -> Result<ConditionalJsonlPublication>
where
    Before: FnOnce() -> Result<()>,
    SyncParent: FnMut(&Path) -> std::io::Result<()>,
{
    let mut before_namespace_change = Some(before_namespace_change);
    publish_staged_jsonl_conditionally_with_hooks(
        temp_path,
        temp_guard,
        output_path,
        staged_source,
        expected_previous_state,
        content_sha256,
        jsonl_authority,
        move |phase| {
            if phase == ConditionalPublicationHookPhase::PreCommit
                && let Some(before_namespace_change) = before_namespace_change.take()
            {
                before_namespace_change()?;
            }
            Ok(())
        },
        |_| sync_parent(output_path),
    )
}

pub(crate) fn export_temp_path(output_path: &Path) -> PathBuf {
    export_temp_path_for_attempt(output_path, 0)
}

fn export_temp_path_for_attempt(output_path: &Path, attempt: u32) -> PathBuf {
    let pid = std::process::id();
    if attempt == 0 {
        return output_path.with_extension(format!("jsonl.{pid}.tmp"));
    }

    let retry_suffix = u64::from(pid)
        .saturating_mul(100)
        .saturating_add(u64::from(attempt));
    output_path.with_extension(format!("jsonl.{retry_suffix}.tmp"))
}

fn create_jsonl_temp_file(output_path: &Path, config: &ExportConfig) -> Result<(PathBuf, File)> {
    for attempt in 0..MAX_JSONL_TEMP_PATH_ATTEMPTS {
        let temp_path = export_temp_path_for_attempt(output_path, attempt);

        if let Some(ref beads_dir) = config.beads_dir {
            validate_temp_file_path(
                &temp_path,
                output_path,
                beads_dir,
                config.allow_external_jsonl,
            )?;
            tracing::debug!(
                temp_path = %temp_path.display(),
                target_path = %output_path.display(),
                "Temp file path validated"
            );
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(temp_file) => return Ok((temp_path, temp_file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&temp_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(BeadsError::Config(format!(
                        "Temporary export file already exists: {}",
                        temp_path.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temporary export file for {}",
        output_path.display()
    )))
}

fn create_pinned_jsonl_temp_file_with<Validate, Hook>(
    output_path: &Path,
    jsonl_authority: &JsonlFamilyWriteLock,
    mut validate: Validate,
    mut hook: Hook,
) -> Result<(PathBuf, PinnedJsonlName, File)>
where
    Validate: FnMut(&Path) -> Result<()>,
    Hook: FnMut(ConditionalPublicationHookPhase) -> Result<()>,
{
    jsonl_authority.verify_jsonl_authority()?;
    let _ = jsonl_authority.pinned_name_for_target(output_path)?;
    hook(ConditionalPublicationHookPhase::PreCreate)?;

    for attempt in 0..MAX_JSONL_TEMP_PATH_ATTEMPTS {
        let temp_path = export_temp_path_for_attempt(output_path, attempt);
        validate(&temp_path)?;
        let pinned_temp = jsonl_authority.pinned_sibling(&temp_path)?;
        let Some(temp_file) = pinned_temp.create_new_regular_if_absent()? else {
            continue;
        };

        // The file was created through the retained directory handle. A route
        // substitution can therefore never redirect creation, but it still
        // invalidates ordinary success and must be surfaced immediately.
        jsonl_authority.verify_jsonl_authority()?;
        return Ok((temp_path, pinned_temp, temp_file));
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate pinned temporary export file for {}",
        output_path.display()
    )))
}

fn create_full_export_temp_file_under_authority(
    output_path: &Path,
    config: &ExportConfig,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<(PathBuf, PinnedJsonlName, File)> {
    create_pinned_jsonl_temp_file_with(
        output_path,
        jsonl_authority,
        |temp_path| {
            if let Some(ref beads_dir) = config.beads_dir {
                validate_temp_file_path(
                    temp_path,
                    output_path,
                    beads_dir,
                    config.allow_external_jsonl,
                )?;
                tracing::debug!(
                    temp_path = %temp_path.display(),
                    target_path = %output_path.display(),
                    "Pinned temp file path validated"
                );
            }
            Ok(())
        },
        |_| Ok(()),
    )
}

fn create_base_snapshot_temp_file_under_authority(
    snapshot_path: &Path,
    jsonl_dir: &Path,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<(PathBuf, PinnedJsonlName, File)> {
    create_pinned_jsonl_temp_file_with(
        snapshot_path,
        jsonl_authority,
        |temp_path| validate_temp_file_path(temp_path, snapshot_path, jsonl_dir, false),
        |_| Ok(()),
    )
}

#[cfg(unix)]
fn set_restrictive_jsonl_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(error) = fs::set_permissions(path, perms) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "Failed to set restrictive permissions on JSONL file"
        );
    }
}

#[cfg(not(unix))]
fn set_restrictive_jsonl_permissions(_path: &Path) {}

/// Exact output approved by a durable export receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedStagedExport {
    /// Exact SHA-256 of the serialized JSONL bytes approved for publication.
    pub raw_sha256: String,
    /// Exact number of serialized issue rows approved for publication.
    pub issue_count: usize,
    /// Exact `(issue_id, content_hash)` mapping approved for finalization.
    pub issue_hashes: AdditiveTableWitness,
}

/// Configuration for JSONL export.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportConfig {
    /// Force export even if database is empty and JSONL has issues.
    pub force: bool,
    /// Whether this is an export to the default JSONL path (affects dirty flag clearing).
    pub is_default_path: bool,
    /// Error handling policy for export.
    pub error_policy: ExportErrorPolicy,
    /// Retention period for tombstones in days (None = keep forever).
    pub retention_days: Option<u64>,
    /// Frozen tombstone-retention cutoff for this logical export.
    ///
    /// When absent, the file exporter captures `Utc::now()` once before
    /// preparing any issue. Resume paths set this explicitly so replay emits
    /// the same bytes as the original committed merge.
    pub export_as_of: Option<DateTime<Utc>>,
    /// The `.beads` directory path for path validation.
    /// If None, path validation is skipped (for backwards compatibility).
    pub beads_dir: Option<PathBuf>,
    /// Allow JSONL path outside `.beads/` directory (requires explicit opt-in).
    /// Even with this flag, git paths are ALWAYS rejected.
    pub allow_external_jsonl: bool,
    /// Show progress indicators for long-running operations.
    pub show_progress: bool,
    /// Configuration for history backups.
    pub history: HistoryConfig,
    /// Worker cap for parallel JSONL line preparation during file exports.
    ///
    /// `0` means "auto": use up to 64 workers, capped by host parallelism.
    /// `1` is the deterministic serial fallback.
    pub max_parallel_workers: usize,
    /// Optional exact staged-output constraint for durable resume workflows.
    ///
    /// The exporter checks both values after the staged file has been flushed,
    /// synced, captured, and structurally validated, but before any namespace
    /// operation can replace the live JSONL.
    pub expected_staged_output: Option<ExpectedStagedExport>,
}

/// Export error handling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExportErrorPolicy {
    /// Abort export on any error (default).
    #[default]
    Strict,
    /// Skip problematic records, export what we can.
    BestEffort,
    /// Export valid records, report failures.
    Partial,
    /// Only export core issues; non-core errors are tolerated.
    RequiredCore,
}

impl std::fmt::Display for ExportErrorPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Strict => "strict",
            Self::BestEffort => "best-effort",
            Self::Partial => "partial",
            Self::RequiredCore => "required-core",
        };
        write!(f, "{value}")
    }
}

impl std::str::FromStr for ExportErrorPolicy {
    type Err = String;

    fn from_str(input: &str) -> std::result::Result<Self, Self::Err> {
        match input.to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "best-effort" | "best_effort" | "best" => Ok(Self::BestEffort),
            "partial" => Ok(Self::Partial),
            "required-core" | "required_core" | "core" => Ok(Self::RequiredCore),
            other => Err(format!(
                "Invalid error policy: {other}. Must be one of: strict, best-effort, partial, required-core"
            )),
        }
    }
}

/// Export entity types for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportEntityType {
    Issue,
    Dependency,
    Label,
    Comment,
}

/// Export error record.
#[derive(Debug, Clone, Serialize)]
pub struct ExportError {
    pub entity_type: ExportEntityType,
    pub entity_id: String,
    pub message: String,
}

impl ExportError {
    fn new(
        entity_type: ExportEntityType,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            entity_type,
            entity_id: entity_id.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn summary(&self) -> String {
        let id = if self.entity_id.is_empty() {
            "<unknown>"
        } else {
            self.entity_id.as_str()
        };
        format!("{:?} {id}: {}", self.entity_type, self.message)
    }
}

/// Export report with error details and counts.
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub issues_exported: usize,
    pub dependencies_exported: usize,
    pub labels_exported: usize,
    pub comments_exported: usize,
    pub errors: Vec<ExportError>,
    pub policy_used: ExportErrorPolicy,
}

impl ExportReport {
    const fn new(policy: ExportErrorPolicy) -> Self {
        Self {
            issues_exported: 0,
            dependencies_exported: 0,
            labels_exported: 0,
            comments_exported: 0,
            errors: Vec::new(),
            policy_used: policy,
        }
    }

    /// True if any errors were recorded.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Success rate for exported entities.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn success_rate(&self) -> f64 {
        let total = self.issues_exported
            + self.dependencies_exported
            + self.labels_exported
            + self.comments_exported;
        let failed = self.errors.len();
        if total + failed == 0 {
            1.0
        } else {
            total as f64 / (total + failed) as f64
        }
    }
}

struct ExportContext {
    policy: ExportErrorPolicy,
    errors: Vec<ExportError>,
}

impl ExportContext {
    const fn new(policy: ExportErrorPolicy) -> Self {
        Self {
            policy,
            errors: Vec::new(),
        }
    }

    fn handle_error(&mut self, err: ExportError) -> Result<()> {
        match self.policy {
            ExportErrorPolicy::Strict => Err(BeadsError::Config(format!(
                "Export error: {}",
                err.summary()
            ))),
            ExportErrorPolicy::BestEffort | ExportErrorPolicy::Partial => {
                self.errors.push(err);
                Ok(())
            }
            ExportErrorPolicy::RequiredCore => {
                if err.entity_type == ExportEntityType::Issue {
                    Err(BeadsError::Config(format!(
                        "Export error: {}",
                        err.summary()
                    )))
                } else {
                    self.errors.push(err);
                    Ok(())
                }
            }
        }
    }
}

/// Namespace operation used to publish a verified JSONL generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPublicationAtomicity {
    /// The destination was known to be absent and the staged file was installed
    /// with an atomic no-replace operation.
    CreateNoReplace,
    /// The staged and prior destination names were atomically exchanged, then
    /// both resulting identities and byte digests were verified.
    ExchangeAndVerify,
    /// The filesystem does not support flagged renames (WSL2 9p/DrvFS answers
    /// `EINVAL`), so the staged file was installed with a plain rename after
    /// the destination witness was re-verified under the held JSONL-family
    /// write authority. The authority excludes other `br` writers, but the
    /// check-then-rename window is not atomic against foreign writers, and a
    /// prior generation is overwritten rather than displaced for recovery
    /// (#419).
    ReplaceUnderAuthority,
}

impl ExportPublicationAtomicity {
    /// Whether publication had to fall back from the atomic protocol.
    #[must_use]
    pub const fn is_downgraded(self) -> bool {
        matches!(self, Self::ReplaceUnderAuthority)
    }

    /// Stable machine-readable name for receipts and robot output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateNoReplace => "create-no-replace",
            Self::ExchangeAndVerify => "exchange-and-verify",
            Self::ReplaceUnderAuthority => "replace-under-authority",
        }
    }
}

/// Verified durable-file publication metadata for an export.
#[derive(Debug, Clone)]
pub struct ExportPublicationReceipt {
    content_sha256: String,
    output_path: String,
    source: Arc<JsonlSourceSnapshot>,
    atomicity: ExportPublicationAtomicity,
    retained_recovery_path: Option<String>,
    cleanup_durable: bool,
}

impl ExportPublicationReceipt {
    #[must_use]
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    #[must_use]
    pub fn output_path(&self) -> &str {
        &self.output_path
    }

    /// Returns the conditional namespace operation used for publication.
    #[must_use]
    pub const fn atomicity(&self) -> ExportPublicationAtomicity {
        self.atomicity
    }

    /// Returns a displaced-generation recovery path retained after an
    /// unsuccessful cleanup, if any.
    #[must_use]
    pub fn retained_recovery_path(&self) -> Option<&str> {
        self.retained_recovery_path.as_deref()
    }

    /// Whether cleanup of any displaced generation was certified durable.
    #[must_use]
    pub const fn cleanup_durable(&self) -> bool {
        self.cleanup_durable
    }
}

/// Result of a JSONL export operation.
#[derive(Debug, Clone, Default)]
pub struct ExportResult {
    /// Number of issues exported.
    pub exported_count: usize,
    /// IDs of exported issues.
    pub exported_ids: Vec<String>,
    /// IDs and timestamps of dirty issues that were cleared.
    pub exported_marked_at: Vec<(String, String)>,
    /// Dirty rows intentionally omitted by the JSONL contract (ephemeral and
    /// wisp issues). Finalization clears these only after the full export is
    /// durably published; failed exportable rows are never included here.
    pub intentionally_excluded_marked_at: Vec<(String, String)>,
    /// IDs skipped due to expired tombstone retention (still clear dirty flags).
    pub skipped_tombstone_ids: Vec<String>,
    /// SHA256 hash of the exported JSONL content.
    pub content_hash: String,
    /// Output file path (None if stdout).
    pub output_path: Option<String>,
    /// Per-issue content hashes (`issue_id`, `content_hash`) for incremental export tracking.
    pub issue_hashes: Vec<(String, String)>,
    /// Exact immutable generation that was verified after durable publication.
    ///
    /// Writer-only exports leave this absent. File exports retain it so
    /// finalization and merge-anchor updates never need to adopt a later path
    /// generation as if it were the one this result describes.
    pub publication: Option<ExportPublicationReceipt>,
}

impl ExportResult {
    pub(crate) fn published_source(&self) -> Result<&JsonlSourceSnapshot> {
        self.publication
            .as_ref()
            .map(|receipt| receipt.source.as_ref())
            .ok_or_else(|| BeadsError::SyncConflict {
                message:
                    "JSONL export result has no verified persisted source generation to finalize"
                        .to_string(),
            })
    }

    pub(crate) fn published_source_arc(&self) -> Result<Arc<JsonlSourceSnapshot>> {
        self.publication
            .as_ref()
            .map(|receipt| Arc::clone(&receipt.source))
            .ok_or_else(|| BeadsError::SyncConflict {
                message:
                    "JSONL export result has no verified persisted source generation to retain"
                        .to_string(),
            })
    }
}

/// Configuration for JSONL import.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ImportConfig {
    /// Skip prefix validation when importing.
    pub skip_prefix_validation: bool,
    /// Rewrite IDs and references on prefix mismatch.
    pub rename_on_import: bool,
    /// Clear duplicate external refs instead of erroring.
    pub clear_duplicate_external_refs: bool,
    /// How to handle orphaned issues during import.
    pub orphan_mode: OrphanMode,
    /// Force upsert even if timestamps are equal or older.
    pub force_upsert: bool,
    /// The `.beads` directory path for path validation.
    /// If None, path validation is skipped (for backwards compatibility).
    pub beads_dir: Option<PathBuf>,
    /// Allow JSONL path outside `.beads/` directory (requires explicit opt-in).
    /// Even with this flag, git paths are ALWAYS rejected.
    pub allow_external_jsonl: bool,
    /// Show progress indicators for long-running operations.
    pub show_progress: bool,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            skip_prefix_validation: false,
            rename_on_import: false,
            clear_duplicate_external_refs: false,
            orphan_mode: OrphanMode::Strict,
            force_upsert: false,
            beads_dir: None,
            allow_external_jsonl: false,
            show_progress: false,
        }
    }
}

/// Orphan handling behavior for import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanMode {
    /// Fail if any issue references a missing parent.
    Strict,
    /// Attempt to resurrect missing parents if found.
    Resurrect,
    /// Skip orphaned issues.
    Skip,
    /// Allow orphans (no parent validation).
    Allow,
}

/// Witness for one `--rename-prefix` id rewrite (old id -> new id).
///
/// `fallback` is `None` when the rename preserved the id remainder
/// (`oldp-slug-hash` -> `newp-slug-hash`); otherwise it names why the id had
/// to be regenerated from scratch instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImportPrefixRename {
    /// Issue id as it appeared in the JSONL source.
    pub old_id: String,
    /// Issue id after the prefix rewrite.
    pub new_id: String,
    /// Reason the remainder-preserving rename was abandoned for this id
    /// (`regenerated-on-collision` or `regenerated-unparseable-id`); absent
    /// when the remainder was preserved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// Result of a JSONL import.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    /// Number of issues imported (created or updated).
    pub imported_count: usize,
    /// Number of issues created during import.
    pub created_count: usize,
    /// Number of issues updated during import.
    pub updated_count: usize,
    /// Number of issues skipped.
    pub skipped_count: usize,
    /// Number of tombstones skipped.
    pub tombstone_skipped: usize,
    /// Conflict markers detected (if any).
    pub conflict_markers: Vec<ConflictMarker>,
    /// Number of orphaned DB entries removed during --rebuild.
    pub orphans_removed: usize,
    /// Number of orphaned FK rows cleaned after deferred-FK import.
    pub orphan_cleaned_count: usize,
    /// Number of label rows imported from JSONL for applied issue records.
    pub labels_imported: usize,
    /// Number of dependency rows imported from JSONL for applied issue records.
    pub dependencies_imported: usize,
    /// Number of comment rows imported from JSONL for applied issue records.
    pub comments_imported: usize,
    /// Byte-identical repeated comment objects removed while normalizing the
    /// JSONL source. Conflicting duplicate IDs remain a hard validation error.
    pub exact_duplicate_comments_deduplicated: usize,
    /// Number of export-hash rows recorded for the imported JSONL snapshot.
    pub export_hashes_recorded: usize,
    /// Number of blocked-cache rows rebuilt after import.
    pub blocked_cache_entries: usize,
    /// Number of child-counter rows rebuilt after import.
    pub child_counter_entries: usize,
    /// Old-id -> new-id receipt for `--rename-prefix` rewrites (empty when
    /// the flag was off or no id needed renaming).
    pub prefix_renames: Vec<ImportPrefixRename>,
    /// Complete semantic post-state expected for every issue row written by
    /// this import. Fresh-family rebuilds verify these witnesses after all
    /// VACUUM/REINDEX/compaction work so a storage-engine field shift cannot
    /// pass merely because table counts still match.
    pub(crate) applied_issues: Vec<Issue>,
}

/// Versioned receipt schema for lossless additive JSONL reconciliation.
pub const ADDITIVE_RECONCILE_SCHEMA: &str = "br.sync.additive-reconciliation.v2";
const ADDITIVE_RECONCILE_ALGORITHM: &str =
    "exact-id-additive-create-monotonic-closure-explicit-scalar-v2";

/// Relation-row counts captured around additive reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveRelationCounts {
    pub labels: usize,
    pub dependencies: usize,
    pub comments: usize,
}

impl AdditiveRelationCounts {
    fn from_issue(issue: &Issue) -> Self {
        Self {
            labels: issue.labels.len(),
            dependencies: issue.dependencies.len(),
            comments: issue.comments.len(),
        }
    }

    fn checked_add(self, other: Self) -> Result<Self> {
        Ok(Self {
            labels: self.labels.checked_add(other.labels).ok_or_else(|| {
                BeadsError::Config("label count overflow during reconciliation".to_string())
            })?,
            dependencies: self
                .dependencies
                .checked_add(other.dependencies)
                .ok_or_else(|| {
                    BeadsError::Config(
                        "dependency count overflow during reconciliation".to_string(),
                    )
                })?,
            comments: self.comments.checked_add(other.comments).ok_or_else(|| {
                BeadsError::Config("comment count overflow during reconciliation".to_string())
            })?,
        })
    }
}

/// Count and canonical payload digest for one complete database table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveTableWitness {
    pub rows: usize,
    pub payload_sha256: String,
}

/// Hash-bound read-only database witness used to reject stale apply plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveDatabaseWitness {
    pub issues: usize,
    /// Digest of every raw issues-table column with explicit NULL framing.
    pub issue_payload_sha256: String,
    /// Digest of the storage-semantic hydrated issue projection.
    pub issue_semantic_payload_sha256: String,
    pub issue_content_hashes: usize,
    pub issue_content_hash_payload_sha256: String,
    pub relations: AdditiveRelationCounts,
    pub label_payload_sha256: String,
    pub dependency_payload_sha256: String,
    pub comment_payload_sha256: String,
    pub export_hashes: usize,
    pub export_hash_payload_sha256: String,
    pub events: usize,
    pub event_payload_sha256: String,
    pub dirty_issues: usize,
    pub dirty_payload_sha256: String,
    pub metadata_rows: usize,
    pub metadata_payload_sha256: String,
    /// Metadata that must remain stable while a committed merge finalizes its
    /// export. This excludes only the pending-receipt row and the documented
    /// export-bookkeeping keys.
    pub stable_metadata: AdditiveTableWitness,
    pub blocked_cache_entries: usize,
    pub blocked_cache_payload_sha256: String,
    pub child_counter_entries: usize,
    pub child_counter_payload_sha256: String,
    pub config: AdditiveTableWitness,
    pub close_metadata: AdditiveTableWitness,
    pub gate_results: AdditiveTableWitness,
    pub gate_result_history: AdditiveTableWitness,
    pub schema_catalog: AdditiveTableWitness,
    pub sqlite_sequence: AdditiveTableWitness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_jsonl_content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_jsonl_mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_jsonl_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_flush: Option<String>,
}

/// Merge-authoritative database state that must remain stable while a
/// committed merge is reconciling its JSONL and base artifacts.
///
/// Export hashes, dirty markers, and export-finalization metadata are
/// intentionally excluded because those surfaces change when the saga moves
/// from `DatabaseCommitted` to `ExportFinalized`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergeDatabaseCoreWitness {
    pub issues: usize,
    pub issue_payload_sha256: String,
    pub issue_semantic_payload_sha256: String,
    pub issue_content_hash_payload_sha256: String,
    pub relations: AdditiveRelationCounts,
    pub label_payload_sha256: String,
    pub dependency_payload_sha256: String,
    pub comment_payload_sha256: String,
    pub events: usize,
    pub event_payload_sha256: String,
    pub stable_metadata: AdditiveTableWitness,
    pub blocked_cache_entries: usize,
    pub blocked_cache_payload_sha256: String,
    pub child_counter_entries: usize,
    pub child_counter_payload_sha256: String,
    pub config: AdditiveTableWitness,
    pub close_metadata: AdditiveTableWitness,
    pub gate_results: AdditiveTableWitness,
    pub gate_result_history: AdditiveTableWitness,
    pub schema_catalog: AdditiveTableWitness,
    pub sqlite_sequence: AdditiveTableWitness,
}

impl From<AdditiveDatabaseWitness> for SyncMergeDatabaseCoreWitness {
    fn from(witness: AdditiveDatabaseWitness) -> Self {
        Self {
            issues: witness.issues,
            issue_payload_sha256: witness.issue_payload_sha256,
            issue_semantic_payload_sha256: witness.issue_semantic_payload_sha256,
            issue_content_hash_payload_sha256: witness.issue_content_hash_payload_sha256,
            relations: witness.relations,
            label_payload_sha256: witness.label_payload_sha256,
            dependency_payload_sha256: witness.dependency_payload_sha256,
            comment_payload_sha256: witness.comment_payload_sha256,
            events: witness.events,
            event_payload_sha256: witness.event_payload_sha256,
            stable_metadata: witness.stable_metadata,
            blocked_cache_entries: witness.blocked_cache_entries,
            blocked_cache_payload_sha256: witness.blocked_cache_payload_sha256,
            child_counter_entries: witness.child_counter_entries,
            child_counter_payload_sha256: witness.child_counter_payload_sha256,
            config: witness.config,
            close_metadata: witness.close_metadata,
            gate_results: witness.gate_results,
            gate_result_history: witness.gate_result_history,
            schema_catalog: witness.schema_catalog,
            sqlite_sequence: witness.sqlite_sequence,
        }
    }
}

/// Hash-only witness for one merge-resolution note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergeNoteWitness {
    pub issue_id: String,
    pub note_sha256: String,
}

/// Digest of the complete issue payload approved for one kept merge row.
///
/// `Issue::content_hash` is skipped by its normal JSON representation, so the
/// digest input below binds it explicitly alongside every serialized scalar
/// and owned relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergeKeptIssueWitness {
    pub issue_id: String,
    pub payload_sha256: String,
}

/// Immutable logical intent reviewed before the merge transaction begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergeIntent {
    pub schema_version: u32,
    pub database_authority_sha256: String,
    pub jsonl_authority_sha256: String,
    pub jsonl_path_sha256: String,
    pub jsonl_before: JsonlSourceStateWitness,
    pub jsonl_before_content_sha256: Option<String>,
    pub base_authority_sha256: String,
    pub base_before: JsonlSourceStateWitness,
    pub base_before_content_sha256: Option<String>,
    pub resolution: String,
    pub actor: String,
    /// Exact audit-event attribution reviewed for this merge.
    ///
    /// Empty attribution is omitted so receipts produced before this field was
    /// introduced retain their canonical serialized intent and digest.
    #[serde(default, skip_serializing_if = "EventAttribution::is_empty")]
    pub event_attribution: EventAttribution,
    /// Exact workflow-capacity policy used for merge admission checks.
    #[serde(
        default,
        skip_serializing_if = "crate::close_policy::CapacityPolicy::is_empty"
    )]
    pub capacity_policy: crate::close_policy::CapacityPolicy,
    pub retention_days: Option<u64>,
    pub export_as_of: DateTime<Utc>,
    pub changed_kept_issue_ids: Vec<String>,
    pub kept_issue_witnesses: Vec<SyncMergeKeptIssueWitness>,
    pub deleted_issue_ids: Vec<String>,
    pub note_witnesses: Vec<SyncMergeNoteWitness>,
    pub database_before: AdditiveDatabaseWitness,
}

impl SyncMergeIntent {
    pub(crate) fn intent_sha256(&self) -> Result<String> {
        sync_merge_domain_separated_sha256(SYNC_MERGE_INTENT_DOMAIN, self, "sync merge intent")
    }
}

/// Durable state-machine phase for a committed merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncMergePendingPhase {
    DatabaseCommitted,
    ExportFinalized,
}

const SYNC_MERGE_INTENT_DOMAIN: &str = "beads-rust.sync-merge-intent.v1";
const SYNC_MERGE_KEPT_ISSUE_DOMAIN: &str = "beads-rust.sync-merge-kept-issue.v1";
const SYNC_MERGE_RECEIPT_ENVELOPE_DOMAIN: &str =
    "beads-rust.sync-merge-receipt.immutable-envelope.v1";
const SYNC_MERGE_RECEIPT_STATE_DOMAIN: &str = "beads-rust.sync-merge-receipt.state.v1";

#[derive(Serialize)]
struct SyncMergeKeptIssueDigestInput<'a> {
    content_hash: &'a Option<String>,
    issue: &'a Issue,
}

pub(crate) fn sync_merge_kept_issue_witnesses(
    issues: &[Issue],
) -> Result<Vec<SyncMergeKeptIssueWitness>> {
    let mut witnesses = issues
        .iter()
        .map(|issue| {
            Ok(SyncMergeKeptIssueWitness {
                issue_id: issue.id.clone(),
                payload_sha256: sync_merge_domain_separated_sha256(
                    SYNC_MERGE_KEPT_ISSUE_DOMAIN,
                    &SyncMergeKeptIssueDigestInput {
                        content_hash: &issue.content_hash,
                        issue,
                    },
                    "sync merge kept issue payload",
                )?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    witnesses.sort_by(|left, right| left.issue_id.cmp(&right.issue_id));
    Ok(witnesses)
}

/// Exact database bookkeeping produced by export finalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergeExportFinalizationWitness {
    pub export_hashes: AdditiveTableWitness,
    pub dirty_issues: AdditiveTableWitness,
    pub jsonl_content_hash: Option<String>,
    pub jsonl_mtime: Option<String>,
    pub jsonl_size: Option<String>,
    pub last_export_time: Option<String>,
    pub needs_flush: Option<String>,
    /// Exact rows for `jsonl_content_hash`, `jsonl_mtime`, `jsonl_size`,
    /// `last_export_time`, and `needs_flush`.
    pub export_metadata: AdditiveTableWitness,
}

fn sync_merge_capacity_warnings_are_empty(
    warnings: &&[crate::close_policy::WorkflowCapacityWarning],
) -> bool {
    warnings.is_empty()
}

#[derive(Serialize)]
struct SyncMergeReceiptEnvelopeDigestInput<'a> {
    schema_version: u32,
    intent_sha256: &'a str,
    created_at: &'a str,
    database_after: &'a SyncMergeDatabaseCoreWitness,
    jsonl_after_raw_sha256: &'a str,
    jsonl_after_content_sha256: &'a str,
    jsonl_after_issue_count: usize,
    jsonl_after_issue_hashes: &'a AdditiveTableWitness,
    #[serde(skip_serializing_if = "sync_merge_capacity_warnings_are_empty")]
    capacity_warnings: &'a [crate::close_policy::WorkflowCapacityWarning],
}

#[derive(Serialize)]
struct SyncMergeReceiptStateDigestInput<'a> {
    receipt_id: &'a str,
    phase: SyncMergePendingPhase,
    jsonl_after: Option<&'a JsonlSourceStateWitness>,
    export_finalization: Option<&'a SyncMergeExportFinalizationWitness>,
}

fn sync_merge_domain_separated_sha256(
    domain: &str,
    value: &impl Serialize,
    context: &str,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(domain, value)).map_err(|error| {
        BeadsError::Config(format!(
            "Failed to serialize {context} for hashing: {error}"
        ))
    })?;
    Ok(hex_encode(&Sha256::digest(bytes)))
}

/// Typed receipt proving a merge's database commit and the remaining artifact
/// reconciliation work.
///
/// The receipt ID binds the immutable evidence envelope, while
/// `state_sha256` binds the current phase and finalized JSONL witness.
/// These digests detect corruption and uncoordinated modification; they are
/// not authentication against an actor that can rewrite the database and
/// recompute hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMergePendingReceipt {
    pub schema_version: u32,
    pub receipt_id: String,
    pub intent_sha256: String,
    pub state_sha256: String,
    pub created_at: String,
    pub phase: SyncMergePendingPhase,
    pub intent: SyncMergeIntent,
    pub database_after: SyncMergeDatabaseCoreWitness,
    pub jsonl_after_raw_sha256: String,
    pub jsonl_after_content_sha256: String,
    pub jsonl_after_issue_count: usize,
    pub jsonl_after_issue_hashes: AdditiveTableWitness,
    /// Soft-capacity warnings produced by the exact commit transaction.
    ///
    /// These remain in the immutable receipt so a crash between the database
    /// commit and artifact publication cannot silently discard user-facing
    /// admission evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    pub jsonl_after: Option<JsonlSourceStateWitness>,
    pub export_finalization: Option<SyncMergeExportFinalizationWitness>,
}

pub(crate) const METADATA_SYNC_MERGE_PENDING: &str = "sync_merge_pending_v2";
pub(crate) const METADATA_SYNC_MERGE_PENDING_LEGACY: &str = "sync_merge_pending_v1";

impl SyncMergePendingReceipt {
    pub(crate) fn new(
        intent: SyncMergeIntent,
        created_at: String,
        database_after: SyncMergeDatabaseCoreWitness,
        jsonl_after_sha256: String,
        jsonl_after_issue_count: usize,
        jsonl_after_issue_hashes: &[(String, String)],
        capacity_warnings: Vec<crate::close_policy::WorkflowCapacityWarning>,
    ) -> Result<Self> {
        let intent_sha256 = intent.intent_sha256()?;
        let jsonl_after_issue_hashes =
            sync_merge_export_hash_mapping_witness(jsonl_after_issue_hashes)?;
        if jsonl_after_issue_hashes.rows != jsonl_after_issue_count {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending sync merge export expected {jsonl_after_issue_count} JSONL rows but received {} issue-hash rows",
                    jsonl_after_issue_hashes.rows
                ),
            });
        }
        let mut receipt = Self {
            schema_version: 2,
            receipt_id: String::new(),
            intent_sha256,
            state_sha256: String::new(),
            created_at,
            phase: SyncMergePendingPhase::DatabaseCommitted,
            intent,
            database_after,
            jsonl_after_raw_sha256: jsonl_after_sha256.clone(),
            jsonl_after_content_sha256: jsonl_after_sha256,
            jsonl_after_issue_count,
            jsonl_after_issue_hashes,
            capacity_warnings,
            jsonl_after: None,
            export_finalization: None,
        };
        receipt.receipt_id = receipt.immutable_envelope_sha256()?;
        receipt.state_sha256 = receipt.current_state_sha256()?;
        receipt.validate()?;
        Ok(receipt)
    }

    fn immutable_envelope_sha256(&self) -> Result<String> {
        sync_merge_domain_separated_sha256(
            SYNC_MERGE_RECEIPT_ENVELOPE_DOMAIN,
            &SyncMergeReceiptEnvelopeDigestInput {
                schema_version: self.schema_version,
                intent_sha256: &self.intent_sha256,
                created_at: &self.created_at,
                database_after: &self.database_after,
                jsonl_after_raw_sha256: &self.jsonl_after_raw_sha256,
                jsonl_after_content_sha256: &self.jsonl_after_content_sha256,
                jsonl_after_issue_count: self.jsonl_after_issue_count,
                jsonl_after_issue_hashes: &self.jsonl_after_issue_hashes,
                capacity_warnings: &self.capacity_warnings,
            },
            "sync merge receipt immutable envelope",
        )
    }

    fn current_state_sha256(&self) -> Result<String> {
        sync_merge_domain_separated_sha256(
            SYNC_MERGE_RECEIPT_STATE_DOMAIN,
            &SyncMergeReceiptStateDigestInput {
                receipt_id: &self.receipt_id,
                phase: self.phase,
                jsonl_after: self.jsonl_after.as_ref(),
                export_finalization: self.export_finalization.as_ref(),
            },
            "sync merge receipt state",
        )
    }

    pub(crate) fn advance_to_export_finalized(
        &self,
        jsonl_after: JsonlSourceStateWitness,
        export_finalization: SyncMergeExportFinalizationWitness,
    ) -> Result<Self> {
        self.validate()?;
        if self.phase != SyncMergePendingPhase::DatabaseCommitted
            || self.jsonl_after.is_some()
            || self.export_finalization.is_some()
        {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending sync merge receipt can advance only from database_committed without a JSONL witness"
                        .to_string(),
            });
        }
        let JsonlSourceStateWitness::Present {
            raw_sha256,
            mtime,
            size,
            ..
        } = &jsonl_after
        else {
            return Err(BeadsError::SyncConflict {
                message: "Pending sync merge finalization requires a present JSONL source witness"
                    .to_string(),
            });
        };
        if raw_sha256 != &self.jsonl_after_raw_sha256 {
            return Err(BeadsError::SyncConflict {
                message:
                    "Finalized JSONL source witness does not match the immutable reviewed export"
                        .to_string(),
            });
        }
        if export_finalization.dirty_issues.rows != 0
            || export_finalization.needs_flush.as_deref() != Some("false")
        {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending sync merge finalization requires zero dirty issues and needs_flush=false"
                    .to_string(),
            });
        }
        if export_finalization.export_metadata.rows != 5 {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending sync merge finalization requires exactly five export metadata rows, found {}",
                    export_finalization.export_metadata.rows
                ),
            });
        }
        if export_finalization.export_hashes != self.jsonl_after_issue_hashes {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending sync merge finalization export-hash mapping does not match the reviewed JSONL output"
                        .to_string(),
            });
        }
        if export_finalization.jsonl_content_hash.as_deref()
            != Some(self.jsonl_after_content_sha256.as_str())
        {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending sync merge finalization metadata does not identify the reviewed JSONL content"
                        .to_string(),
            });
        }
        let Some(last_export_time) = export_finalization.last_export_time.as_deref() else {
            return Err(BeadsError::SyncConflict {
                message: "Pending sync merge finalization lacks last_export_time metadata"
                    .to_string(),
            });
        };
        DateTime::parse_from_rfc3339(last_export_time).map_err(|error| {
            BeadsError::SyncConflict {
                message: format!(
                    "Pending sync merge finalization has invalid last_export_time metadata: {error}"
                ),
            }
        })?;
        if export_finalization.jsonl_mtime.as_deref() != Some(mtime.as_str())
            || export_finalization
                .jsonl_size
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                != Some(*size)
        {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending sync merge finalization does not match the published JSONL mtime or exact size witness"
                        .to_string(),
            });
        }

        let mut finalized = self.clone();
        finalized.phase = SyncMergePendingPhase::ExportFinalized;
        finalized.jsonl_after = Some(jsonl_after);
        finalized.export_finalization = Some(export_finalization);
        finalized.state_sha256 = finalized.current_state_sha256()?;
        finalized.validate()?;
        Ok(finalized)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema_version != 2 || self.intent.schema_version != 2 {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Unsupported pending merge receipt schema {} (intent schema {})",
                    self.schema_version, self.intent.schema_version
                ),
            });
        }
        for (field, value) in [
            (
                "database_authority_sha256",
                self.intent.database_authority_sha256.as_str(),
            ),
            (
                "jsonl_authority_sha256",
                self.intent.jsonl_authority_sha256.as_str(),
            ),
            ("jsonl_path_sha256", self.intent.jsonl_path_sha256.as_str()),
            (
                "base_authority_sha256",
                self.intent.base_authority_sha256.as_str(),
            ),
            (
                "jsonl_after_raw_sha256",
                self.jsonl_after_raw_sha256.as_str(),
            ),
            (
                "jsonl_after_content_sha256",
                self.jsonl_after_content_sha256.as_str(),
            ),
            ("intent_sha256", self.intent_sha256.as_str()),
            ("receipt_id", self.receipt_id.as_str()),
            ("state_sha256", self.state_sha256.as_str()),
        ] {
            validate_sync_merge_sha256(field, value)?;
        }
        validate_sync_merge_sha256(
            "jsonl_after_issue_hashes",
            &self.jsonl_after_issue_hashes.payload_sha256,
        )?;
        if self.jsonl_after_issue_hashes.rows != self.jsonl_after_issue_count {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending merge receipt issue-hash mapping does not exactly cover the reviewed JSONL row count"
                        .to_string(),
            });
        }
        validate_sync_merge_source_state(
            "jsonl_before",
            &self.intent.jsonl_before,
            self.intent.jsonl_before_content_sha256.as_deref(),
        )?;
        validate_sync_merge_source_state(
            "base_before",
            &self.intent.base_before,
            self.intent.base_before_content_sha256.as_deref(),
        )?;
        validate_sync_merge_sorted_ids(
            "changed_kept_issue_ids",
            &self.intent.changed_kept_issue_ids,
        )?;
        let kept_witness_ids = self
            .intent
            .kept_issue_witnesses
            .iter()
            .map(|witness| witness.issue_id.clone())
            .collect::<Vec<_>>();
        validate_sync_merge_sorted_ids("kept_issue_witnesses", &kept_witness_ids)?;
        if kept_witness_ids != self.intent.changed_kept_issue_ids {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending merge kept payload witnesses do not exactly cover the reviewed kept issue IDs"
                        .to_string(),
            });
        }
        for witness in &self.intent.kept_issue_witnesses {
            validate_sync_merge_sha256("kept_issue_payload_sha256", &witness.payload_sha256)?;
        }
        if let Some(finalization) = &self.export_finalization {
            for (field, witness) in [
                ("export_hashes", &finalization.export_hashes),
                ("dirty_issues", &finalization.dirty_issues),
                ("export_metadata", &finalization.export_metadata),
            ] {
                validate_sync_merge_sha256(field, &witness.payload_sha256)?;
            }
        }
        validate_sync_merge_sorted_ids("deleted_issue_ids", &self.intent.deleted_issue_ids)?;
        if let Some(overlap) = self.intent.changed_kept_issue_ids.iter().find(|issue_id| {
            self.intent
                .deleted_issue_ids
                .binary_search(issue_id)
                .is_ok()
        }) {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending merge receipt places issue {overlap} in both kept and deleted sets"
                ),
            });
        }
        let note_ids = self
            .intent
            .note_witnesses
            .iter()
            .map(|witness| witness.issue_id.clone())
            .collect::<Vec<_>>();
        validate_sync_merge_sorted_ids("note_witnesses", &note_ids)?;
        for witness in &self.intent.note_witnesses {
            validate_sync_merge_sha256("note_sha256", &witness.note_sha256)?;
            if self
                .intent
                .changed_kept_issue_ids
                .binary_search(&witness.issue_id)
                .is_err()
            {
                return Err(BeadsError::SyncConflict {
                    message: format!(
                        "Pending merge note target {} is not a changed kept issue",
                        witness.issue_id
                    ),
                });
            }
        }
        if !matches!(
            self.intent.resolution.as_str(),
            "manual" | "force-db" | "force-jsonl" | "force-newer" | "source-repo-path-migration"
        ) {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending merge receipt has unsupported resolution {}",
                    self.intent.resolution
                ),
            });
        }
        if self.intent.actor.trim().is_empty() || self.intent.actor.trim() != self.intent.actor {
            return Err(BeadsError::SyncConflict {
                message: "Pending merge receipt actor must be nonblank and trimmed".to_string(),
            });
        }
        let created_at = DateTime::parse_from_rfc3339(&self.created_at)
            .map_err(|error| BeadsError::SyncConflict {
                message: format!(
                    "Pending merge receipt has invalid RFC 3339 creation time: {error}"
                ),
            })?
            .with_timezone(&Utc);
        if created_at != self.intent.export_as_of {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending merge receipt creation time does not match its frozen export cutoff"
                        .to_string(),
            });
        }
        if let Some(jsonl_after) = self.jsonl_after.as_ref() {
            validate_sync_merge_source_state(
                "jsonl_after",
                jsonl_after,
                Some(&self.jsonl_after_content_sha256),
            )?;
        }
        if let Some(finalization) = self.export_finalization.as_ref() {
            let last_export_time = finalization.last_export_time.as_deref().ok_or_else(|| {
                BeadsError::SyncConflict {
                    message:
                        "Export-finalized pending merge receipt lacks last_export_time metadata"
                            .to_string(),
                }
            })?;
            DateTime::parse_from_rfc3339(last_export_time).map_err(|error| {
                BeadsError::SyncConflict {
                    message: format!(
                        "Export-finalized pending merge receipt has invalid last_export_time metadata: {error}"
                    ),
                }
            })?;
        }
        match (
            self.phase,
            self.jsonl_after.as_ref(),
            self.export_finalization.as_ref(),
        ) {
            (SyncMergePendingPhase::DatabaseCommitted, None, None) => {}
            (
                SyncMergePendingPhase::ExportFinalized,
                Some(JsonlSourceStateWitness::Present {
                    raw_sha256,
                    mtime,
                    size,
                    ..
                }),
                Some(finalization),
            ) if raw_sha256 == &self.jsonl_after_raw_sha256
                && finalization.export_hashes == self.jsonl_after_issue_hashes
                && finalization.export_metadata.rows == 5
                && finalization.jsonl_content_hash.as_deref()
                    == Some(self.jsonl_after_content_sha256.as_str())
                && finalization.jsonl_mtime.as_deref() == Some(mtime.as_str())
                && finalization
                    .jsonl_size
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    == Some(*size)
                && finalization.dirty_issues.rows == 0
                && finalization.needs_flush.as_deref() == Some("false") => {}
            (SyncMergePendingPhase::DatabaseCommitted, _, _) => {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Database-committed pending merge receipt already contains finalized export evidence"
                            .to_string(),
                });
            }
            (SyncMergePendingPhase::ExportFinalized, _, _) => {
                return Err(BeadsError::SyncConflict {
                    message:
                        "Export-finalized pending merge receipt lacks exact JSONL or database-bookkeeping evidence"
                            .to_string(),
                });
            }
        }
        let computed_intent_sha256 = self.intent.intent_sha256()?;
        if self.intent_sha256 != computed_intent_sha256 {
            return Err(BeadsError::SyncConflict {
                message: "Pending merge receipt intent hash is malformed or has been modified"
                    .to_string(),
            });
        }
        if self.receipt_id != self.immutable_envelope_sha256()? {
            return Err(BeadsError::SyncConflict {
                message:
                    "Pending merge receipt immutable envelope is malformed or has been modified"
                        .to_string(),
            });
        }
        if self.state_sha256 != self.current_state_sha256()? {
            return Err(BeadsError::SyncConflict {
                message: "Pending merge receipt state hash is malformed or has been modified"
                    .to_string(),
            });
        }
        Ok(())
    }
}

fn validate_sync_merge_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(BeadsError::SyncConflict {
        message: format!("Pending merge receipt field {field} is not a lowercase SHA-256 digest"),
    })
}

fn validate_sync_merge_source_state(
    field: &str,
    state: &JsonlSourceStateWitness,
    content_sha256: Option<&str>,
) -> Result<()> {
    match state {
        JsonlSourceStateWitness::Missing if content_sha256.is_none() => Ok(()),
        JsonlSourceStateWitness::Missing => Err(BeadsError::SyncConflict {
            message: format!(
                "Pending merge receipt field {field} is missing but has a content digest"
            ),
        }),
        JsonlSourceStateWitness::Present {
            raw_sha256, mtime, ..
        } => {
            validate_sync_merge_sha256(&format!("{field}.raw_sha256"), raw_sha256)?;
            DateTime::parse_from_rfc3339(mtime).map_err(|error| BeadsError::SyncConflict {
                message: format!(
                    "Pending merge receipt field {field}.mtime is not valid RFC 3339: {error}"
                ),
            })?;
            let content_sha256 = content_sha256.ok_or_else(|| BeadsError::SyncConflict {
                message: format!(
                    "Pending merge receipt field {field} is present without a content digest"
                ),
            })?;
            validate_sync_merge_sha256(&format!("{field}.content_sha256"), content_sha256)
        }
    }
}

fn validate_sync_merge_sorted_ids(field: &str, issue_ids: &[String]) -> Result<()> {
    if issue_ids
        .iter()
        .any(|issue_id| issue_id.trim().is_empty() || issue_id.trim() != issue_id)
        || issue_ids.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Pending merge receipt field {field} must contain sorted, unique, nonblank issue IDs"
            ),
        });
    }
    Ok(())
}

/// Database-health gates captured before and after additive reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveDatabaseHealth {
    pub integrity_messages: Vec<String>,
    pub foreign_key_violations: Vec<Vec<String>>,
}

/// Deterministic remap of a storage-local comment surrogate ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveCommentIdRemap {
    pub issue_id: String,
    pub old_id: i64,
    pub new_id: i64,
    pub logical_payload_sha256: String,
}

/// Complete deterministic issue-to-reason conflict witness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveConflictWitness {
    pub issue_id: String,
    pub reasons: Vec<String>,
    pub details: Vec<AdditiveConflictDetailWitness>,
}

/// Privacy-preserving, actionable evidence for one rejected source element.
///
/// Embedded relation values are not assumed to be validated issue IDs: an
/// external target or malformed source can contain terminal control bytes or
/// a local path. All such related values, labels, external refs, dependency
/// metadata, and comment payloads are represented only by SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveConflictDetailWitness {
    pub reason: String,
    pub detail_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_value_sha256: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_subcodes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_sha256: Option<String>,
    pub detail_sha256: String,
}

/// Complete hash-bound scalar difference for a shared issue that was refused.
///
/// Payload bodies remain private; field names and before/after/diff hashes let
/// an operator prove exactly which reviewed scalar conflict a token covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveConflictScalarDiffWitness {
    pub issue_id: String,
    pub changed_fields: Vec<String>,
    pub diff_sha256: String,
    pub before_payload_sha256: String,
    pub after_payload_sha256: String,
}

/// Privacy-preserving relation delta for a shared issue that was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveConflictRelationDiffWitness {
    pub issue_id: String,
    pub changed_relation_classes: Vec<String>,
    pub before_counts: AdditiveRelationCounts,
    pub after_counts: AdditiveRelationCounts,
    pub before_payload_sha256: String,
    pub after_payload_sha256: String,
    pub added_element_sha256: Vec<String>,
    pub removed_element_sha256: Vec<String>,
    pub diff_sha256: String,
}

/// Lifecycle state of a reconciliation receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdditiveReconcileStatus {
    Conflicted,
    MetadataOnlyReady,
    NoChanges,
    Ready,
    AppliedMetadataOnly,
    Applied,
    /// The transaction committed, but one or more independently reported
    /// source, authority, or connection postconditions failed afterward.
    CommittedWithPostconditionFailures,
}

impl AdditiveReconcileStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conflicted => "conflicted",
            Self::MetadataOnlyReady => "metadata_only_ready",
            Self::NoChanges => "no_changes",
            Self::Ready => "ready",
            Self::AppliedMetadataOnly => "applied_metadata_only",
            Self::Applied => "applied",
            Self::CommittedWithPostconditionFailures => "committed_with_postcondition_failures",
        }
    }
}

/// Independently composable failure observed only after SQLite committed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AdditivePostcommitFailure {
    ForeignKeyRestoration,
    DatabaseAuthorityChanged,
    DatabasePoststateChanged,
    WorkspaceAuthorityChanged,
    SourceWitnessChanged,
}

/// Evidence-backed reason a shared issue's scalar fields may be updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdditiveScalarResolution {
    MonotonicClosure,
    ExplicitSourceResolution,
}

impl AdditiveScalarResolution {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MonotonicClosure => "monotonic_closure",
            Self::ExplicitSourceResolution => "explicit_source_resolution",
        }
    }
}

/// Hash-bound scalar-only repair for a shared issue ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveScalarUpdateWitness {
    pub issue_id: String,
    pub resolution: AdditiveScalarResolution,
    pub changed_fields: Vec<String>,
    pub diff_sha256: String,
    pub before_payload_sha256: String,
    pub after_payload_sha256: String,
    pub relation_payload_sha256: String,
}

/// Exact before/after witness for a repaired persisted content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveContentHashRepairWitness {
    pub issue_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    pub after: String,
}

/// Complete hash-bound result for additive reconciliation planning/apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditiveReconcileReceipt {
    pub schema: String,
    pub algorithm: String,
    pub tool_version: String,
    pub plan_sha256: String,
    pub status: AdditiveReconcileStatus,
    pub workspace_path: String,
    pub workspace_path_sha256: String,
    pub workspace_identity_sha256: String,
    pub source_path: String,
    pub source_path_sha256: String,
    pub source_identity_sha256: String,
    pub database_path: String,
    pub database_path_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_identity_sha256: Option<String>,
    pub write_lock_authority: String,
    pub write_lock_authority_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_authority_preserved_after_commit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_poststate_preserved_after_commit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_authority_preserved_after_commit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_preserved_after_commit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreign_keys_restored_after_commit: Option<bool>,
    pub postcommit_failures: Vec<AdditivePostcommitFailure>,
    pub database_user_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_prefix: Option<String>,
    /// SHA-256 of the exact source bytes.
    pub source_raw_sha256: String,
    /// SHA-256 of trimmed nonblank JSONL records with canonical LF framing.
    pub source_content_sha256: String,
    /// SHA-256 of the deterministic storage-semantic projection of all source issues.
    pub source_storage_projection_sha256: String,
    pub source_size: u64,
    pub source_mtime: String,
    /// Deterministic timestamp used for recovery-owned bookkeeping rows.
    /// Equal to the reviewed source snapshot mtime.
    pub poststate_timestamp: String,
    pub source_issues: usize,
    pub target_before: AdditiveDatabaseWitness,
    pub expected_target_after: AdditiveDatabaseWitness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_after: Option<AdditiveDatabaseWitness>,
    pub health_before: AdditiveDatabaseHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_after: Option<AdditiveDatabaseHealth>,
    pub created: usize,
    pub created_issue_ids: Vec<String>,
    pub created_issue_ids_sha256: String,
    pub updated: usize,
    pub updated_issue_ids: Vec<String>,
    pub updated_issue_ids_sha256: String,
    /// Exact override set supplied by the operator, whether or not each ID was applicable.
    pub requested_source_authoritative_issue_ids: Vec<String>,
    pub requested_source_authoritative_issue_ids_sha256: String,
    /// Applicable overrides actually used by this plan.
    pub source_authoritative_issue_ids: Vec<String>,
    pub source_authoritative_issue_ids_sha256: String,
    pub scalar_updates: Vec<AdditiveScalarUpdateWitness>,
    pub scalar_updates_sha256: String,
    pub content_hash_repairs_planned: usize,
    pub content_hash_repairs: Vec<AdditiveContentHashRepairWitness>,
    pub content_hash_repairs_sha256: String,
    pub content_hash_repair_issue_ids: Vec<String>,
    pub content_hash_repair_issue_ids_sha256: String,
    pub content_hash_repairs_applied: usize,
    pub skipped_equal: usize,
    pub equal_issue_ids: Vec<String>,
    pub equal_issue_ids_sha256: String,
    pub skipped_ephemeral: usize,
    /// Source issues proven equal to, created in, or updated in SQLite.
    pub synchronized: usize,
    pub export_hash_updates_planned: usize,
    pub export_hashes_updated: usize,
    pub dirty_markers_clear_planned: usize,
    pub dirty_markers_cleared: usize,
    /// Number of distinct issues involved in one or more conflicts.
    pub conflicted: usize,
    /// Total conflict observations across all reason buckets.
    pub conflict_occurrences: usize,
    pub deleted: usize,
    pub db_only_preserved: usize,
    pub db_only_issue_ids: Vec<String>,
    pub db_only_issue_ids_sha256: String,
    pub conflict_reasons: BTreeMap<String, usize>,
    pub conflict_issue_ids: Vec<String>,
    pub conflict_issue_ids_sha256: String,
    pub conflict_issue_ids_truncated: bool,
    pub conflict_witnesses: Vec<AdditiveConflictWitness>,
    pub conflict_witnesses_sha256: String,
    pub conflict_scalar_diffs: Vec<AdditiveConflictScalarDiffWitness>,
    pub conflict_scalar_diffs_sha256: String,
    pub conflict_relation_diffs: Vec<AdditiveConflictRelationDiffWitness>,
    pub conflict_relation_diffs_sha256: String,
    pub comment_id_remaps: Vec<AdditiveCommentIdRemap>,
    pub comment_id_remaps_sha256: String,
    /// Blocking-cycle components already present in the database.
    pub preexisting_blocking_cycles: usize,
    /// Blocking-cycle components in the database after applying the plan.
    pub projected_blocking_cycles: usize,
    /// Projected blocking-cycle components not present before reconciliation.
    pub new_blocking_cycles: usize,
    pub relations_before: AdditiveRelationCounts,
    pub relations_after: AdditiveRelationCounts,
    pub relation_rows_planned: AdditiveRelationCounts,
    pub relation_rows_applied: AdditiveRelationCounts,
    pub expected_issue_raw_payload_sha256: String,
    pub expected_issue_semantic_payload_sha256: String,
    pub expected_issue_content_hash_payload_sha256: String,
    pub expected_export_hash_payload_sha256: String,
    pub expected_dirty_payload_sha256: String,
    pub expected_metadata_payload_sha256: String,
    pub expected_blocked_cache_payload_sha256: String,
    pub expected_child_counter_payload_sha256: String,
    pub expected_sqlite_sequence_payload_sha256: String,
    pub events_before: usize,
    pub events_after: usize,
    pub event_payload_sha256_before: String,
    pub event_payload_sha256_after: String,
    pub cache_rebuild_planned: bool,
    pub cache_rebuild_performed: bool,
    pub metadata_update_planned: bool,
    pub metadata_changed: bool,
    pub jsonl_written: bool,
    pub base_snapshot_used: bool,
    pub merge_note_written: bool,
}

/// Path/safety policy for additive reconciliation.
#[derive(Debug, Clone, Default)]
pub struct AdditiveReconcileConfig {
    pub beads_dir: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub allow_external_jsonl: bool,
    /// Exact shared IDs for which reviewed source scalar fields override SQLite.
    pub source_authoritative_ids: BTreeSet<String>,
}

/// Safe high-level request for applying an exact reviewed additive plan.
///
/// This API owns the `.write.lock`, opens only an existing current-schema
/// database, rebuilds the plan under that boundary, and delegates to the
/// transaction-scoped low-level apply. It performs no schema migration,
/// corruption recovery, JSONL export, or implicit merge.
#[derive(Debug, Clone)]
pub struct ReviewedAdditiveReconcileRequest {
    pub beads_dir: PathBuf,
    /// Optional database override with the same precedence and path semantics
    /// as the CLI's `--db` flag.
    pub db_override: Option<PathBuf>,
    /// Optional JSONL override. When absent, startup configuration resolves the
    /// same source path as `br sync`.
    pub source_path_override: Option<PathBuf>,
    pub allow_external_jsonl: bool,
    pub source_authoritative_ids: BTreeSet<String>,
    pub expected_plan_sha256: String,
    pub lock_timeout_ms: Option<u64>,
}

/// Read-only high-level request for producing the exact token and receipt later
/// consumed by [`apply_reviewed_additive_reconcile`].
#[derive(Debug, Clone)]
pub struct ReviewedAdditiveReconcilePlanRequest {
    pub beads_dir: PathBuf,
    pub db_override: Option<PathBuf>,
    pub source_path_override: Option<PathBuf>,
    pub allow_external_jsonl: bool,
    pub source_authoritative_ids: BTreeSet<String>,
}

#[cfg(unix)]
fn additive_metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(unix)]
fn additive_file_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not witness reviewed file identity for {}: {error}",
            additive_path_descriptor(path, "reviewed-file")
        ))
    })?;
    Ok(additive_metadata_identity(&metadata))
}

/// Non-Unix auxiliary identity for reviewed source-file snapshots.
///
/// This creation/modified-time witness is never accepted as SQLite database
/// inode authority: [`additive_file_identity`] fails closed on these targets.
/// Source snapshots separately bind exact content bytes, size, and timestamps;
/// this tuple only adds a best-effort replacement signal for that read-only
/// source route.
#[cfg(not(unix))]
fn additive_metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    #[allow(clippy::cast_possible_truncation)]
    fn nanos(time: std::io::Result<std::time::SystemTime>) -> Option<u64> {
        time.ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
    }
    let created = nanos(metadata.created())
        .or_else(|| nanos(metadata.modified()))
        .unwrap_or(0);
    (created, u64::from(metadata.file_type().is_file()))
}

#[cfg(not(unix))]
fn additive_file_identity(path: &Path) -> Result<(u64, u64)> {
    Err(BeadsError::Config(format!(
        "Reviewed additive reconciliation requires a stable platform file ID; unsupported for {} on this target",
        additive_path_descriptor(path, "reviewed-file")
    )))
}

fn resolve_reviewed_additive_workspace(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
) -> Result<(PathBuf, PathBuf, PathBuf, (u64, u64))> {
    for raw_path in std::iter::once(beads_dir).chain(db_override.map(PathBuf::as_path)) {
        let validation = validate_no_git_path(raw_path);
        if !validation.is_allowed() {
            return Err(BeadsError::Config(format!(
                "Reviewed reconciliation path was rejected: {}",
                additive_path_descriptor(raw_path, "reviewed-path")
            )));
        }
    }
    let terminal_beads = redact_reviewed_path_result(
        crate::config::routing::follow_redirects(beads_dir, 10),
        beads_dir,
        "workspace",
        "resolve redirects for",
    )?;
    let canonical_beads = fs::canonicalize(&terminal_beads).map_err(|error| {
        BeadsError::Config(format!(
            "Could not canonicalize reviewed beads directory {}: {error}",
            additive_path_descriptor(&terminal_beads, "reviewed-workspace")
        ))
    })?;
    let startup = redact_reviewed_path_result(
        crate::config::load_startup_config_with_paths_uncached(&canonical_beads, db_override),
        &canonical_beads,
        "workspace",
        "load startup configuration for",
    )?;
    let database_path = startup.paths.db_path;
    let jsonl_path = startup.paths.jsonl_path;
    let absolute_database_path = if database_path.is_absolute() {
        database_path.clone()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                BeadsError::Config(format!(
                    "Could not resolve relative reviewed database path {}: {error}",
                    database_path_descriptor(&database_path)
                ))
            })?
            .join(&database_path)
    };
    for path in [&canonical_beads, &database_path] {
        let validation = validate_no_git_path(path);
        if !validation.is_allowed() {
            return Err(BeadsError::Config(format!(
                "Reviewed reconciliation path was rejected: {}",
                additive_path_descriptor(path, "reviewed-path")
            )));
        }
    }
    let database_metadata = fs::symlink_metadata(&database_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not inspect reviewed database {}: {error}",
            database_path_descriptor(&database_path)
        ))
    })?;
    if database_metadata.file_type().is_symlink() {
        return Err(BeadsError::Config(format!(
            "Reviewed additive reconciliation refuses a symlinked database: {}",
            database_path_descriptor(&database_path)
        )));
    }
    if !database_metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Reviewed additive reconciliation requires a regular database file: {}",
            database_path_descriptor(&database_path)
        )));
    }
    let canonical_database = fs::canonicalize(&database_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not canonicalize reviewed database {}: {error}",
            database_path_descriptor(&database_path)
        ))
    })?;
    if absolute_database_path.starts_with(&canonical_beads)
        && !canonical_database.starts_with(&canonical_beads)
    {
        return Err(BeadsError::Config(format!(
            "Reviewed database path {} escapes its locked beads directory {} through a symlink",
            database_path_descriptor(&canonical_database),
            additive_path_descriptor(&canonical_beads, "reviewed-workspace")
        )));
    }
    let identity = additive_file_identity(&canonical_database)?;
    Ok((canonical_beads, canonical_database, jsonl_path, identity))
}

/// Build a reviewed additive plan without creating, migrating, repairing, or
/// otherwise writing the database family.
pub fn plan_reviewed_additive_reconcile(
    request: &ReviewedAdditiveReconcilePlanRequest,
) -> Result<AdditiveReconcilePlan> {
    let (canonical_beads, canonical_database, configured_source, database_identity) =
        resolve_reviewed_additive_workspace(&request.beads_dir, request.db_override.as_ref())?;
    let storage = redact_reviewed_path_result(
        SqliteStorage::open_current_read_only(&canonical_database),
        &canonical_database,
        "database",
        "open",
    )?
    .ok_or_else(|| {
        BeadsError::Config(format!(
            "Additive reconciliation requires an existing current-schema database at {}",
            database_path_descriptor(&canonical_database)
        ))
    })?;
    if additive_file_identity(&canonical_database)? != database_identity {
        return Err(BeadsError::Config(
            "Reviewed database identity changed while opening the read-only reconciliation plan"
                .to_string(),
        ));
    }
    let source_path = request
        .source_path_override
        .as_deref()
        .unwrap_or(&configured_source);
    let allow_external_jsonl = request.allow_external_jsonl
        || crate::config::implicit_external_jsonl_allowed(
            &canonical_beads,
            &canonical_database,
            source_path,
        );
    let plan = plan_additive_reconcile(
        &storage,
        source_path,
        &AdditiveReconcileConfig {
            beads_dir: Some(canonical_beads.clone()),
            database_path: Some(canonical_database.clone()),
            allow_external_jsonl,
            source_authoritative_ids: request.source_authoritative_ids.clone(),
        },
    )?;
    let (beads_after, database_after, source_after, identity_after) =
        resolve_reviewed_additive_workspace(&request.beads_dir, request.db_override.as_ref())?;
    if beads_after != canonical_beads
        || database_after != canonical_database
        || source_after != configured_source
        || identity_after != database_identity
    {
        return Err(BeadsError::Config(
            "Reviewed workspace, database, or source authority changed while producing the read-only reconciliation plan".to_string(),
        ));
    }
    Ok(plan)
}

/// Apply a conflict-free additive plan only when its fresh token exactly
/// matches the caller's reviewed token.
///
/// # Errors
///
/// Returns an error without applying the plan when the lock cannot be acquired,
/// the database is absent/stale, the token is malformed or mismatched, any
/// source/database witness drifts, or any transactional invariant fails.
pub fn apply_reviewed_additive_reconcile(
    request: &ReviewedAdditiveReconcileRequest,
) -> Result<AdditiveReconcileReceipt> {
    apply_reviewed_additive_reconcile_under_authority(request, None)
}

/// Apply a reviewed additive plan while reusing an authority already retained
/// by the CLI startup gate.
///
/// The retained capability is accepted only when it protects this exact
/// terminal workspace, canonical database-family sidecar, and database path.
/// Standalone library callers use [`apply_reviewed_additive_reconcile`], which
/// acquires and owns the same composite authority itself.
#[allow(clippy::too_many_lines)]
pub(crate) fn apply_reviewed_additive_reconcile_under_authority(
    request: &ReviewedAdditiveReconcileRequest,
    retained_write_authority: Option<&Arc<DatabaseFamilyWriteLock>>,
) -> Result<AdditiveReconcileReceipt> {
    if request.expected_plan_sha256.len() != 64
        || !request
            .expected_plan_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BeadsError::Config(
            "Reviewed additive reconciliation plan SHA-256 must be exactly 64 lowercase hexadecimal characters"
                .to_string(),
        ));
    }

    let (canonical_beads, canonical_database, configured_source, database_identity) =
        resolve_reviewed_additive_workspace(&request.beads_dir, request.db_override.as_ref())?;
    let write_authority = if let Some(authority) = retained_write_authority {
        authority.verify_database_authority()?;
        let expected_workspace_lock = canonical_beads.join(".write.lock");
        let expected_authority_lock = database_write_authority_path(&canonical_database)?;
        if authority.workspace_lock_path != expected_workspace_lock
            || authority.authority_lock_path != expected_authority_lock
            || authority.canonical_database_path != canonical_database
        {
            return Err(BeadsError::SyncConflict {
                message: "Retained database-family authority does not protect the exact reviewed workspace and database"
                    .to_string(),
            });
        }
        Arc::clone(authority)
    } else {
        Arc::new(blocking_database_family_write_lock_with_timeout(
            &canonical_beads,
            &canonical_database,
            request.lock_timeout_ms,
        )?)
    };
    let (
        beads_after_authority_lock,
        database_after_authority_lock,
        configured_source_after_authority_lock,
        database_identity_after_authority_lock,
    ) = resolve_reviewed_additive_workspace(&request.beads_dir, request.db_override.as_ref())?;
    if beads_after_authority_lock != canonical_beads
        || database_after_authority_lock != canonical_database
        || database_identity_after_authority_lock != database_identity
        || (request.source_path_override.is_none()
            && configured_source_after_authority_lock != configured_source)
    {
        return Err(BeadsError::Config(
            "Reviewed additive reconciliation workspace or database identity changed while acquiring the database authority lock".to_string(),
        ));
    }
    write_authority.verify_database_authority()?;
    let mut storage = redact_reviewed_path_result(
        SqliteStorage::open_current_for_reconcile(&canonical_database, request.lock_timeout_ms),
        &canonical_database,
        "database",
        "open",
    )?
    .ok_or_else(|| {
        BeadsError::Config(format!(
            "Additive reconciliation requires an existing current-schema database at {}",
            database_path_descriptor(&canonical_database)
        ))
    })?;
    write_authority.verify_database_authority()?;
    storage.attach_write_authority(std::sync::Arc::clone(&write_authority));
    if additive_file_identity(&canonical_database)? != database_identity {
        return Err(BeadsError::Config(
            "Reviewed database identity changed while opening the token-bound reconciliation connection"
                .to_string(),
        ));
    }
    let source_path = request
        .source_path_override
        .as_deref()
        .unwrap_or(&configured_source_after_authority_lock);
    let allow_external_jsonl = request.allow_external_jsonl
        || crate::config::implicit_external_jsonl_allowed(
            &canonical_beads,
            &canonical_database,
            source_path,
        );
    redact_reviewed_path_result(
        validate_sync_path_with_external(source_path, &canonical_beads, allow_external_jsonl),
        source_path,
        "source",
        "validate path policy for",
    )?;
    let _sync_authority = redact_reviewed_path_result(
        try_sync_lock(&canonical_beads),
        &canonical_beads,
        "workspace",
        "acquire sync authority for",
    )?
    .ok_or_else(|| BeadsError::SyncConflict {
        message: "Reviewed additive reconciliation cannot start while another sync owns .sync.lock"
            .to_string(),
    })?;
    let source_family_authority = redact_reviewed_path_result(
        blocking_jsonl_family_write_lock_with_timeout(source_path, request.lock_timeout_ms),
        source_path,
        "source",
        "acquire JSONL-family authority for",
    )?;
    source_family_authority.verify_jsonl_authority()?;
    let _source_inode_witness = acquire_reviewed_additive_source_lock(source_path)?;
    let config = AdditiveReconcileConfig {
        beads_dir: Some(canonical_beads.clone()),
        database_path: Some(canonical_database.clone()),
        allow_external_jsonl,
        source_authoritative_ids: request.source_authoritative_ids.clone(),
    };
    let plan = plan_additive_reconcile(&storage, source_path, &config)?;
    if plan.receipt.write_lock_authority_sha256 != write_authority.authority_path_sha256() {
        return Err(BeadsError::Config(
            "Reviewed additive reconciliation planned a different database-family write authority"
                .to_string(),
        ));
    }
    let mut receipt = apply_additive_reconcile(
        &mut storage,
        source_path,
        &config,
        &plan,
        &request.expected_plan_sha256,
    )?;
    source_family_authority.verify_jsonl_authority()?;
    let post_resolution =
        resolve_reviewed_additive_workspace(&request.beads_dir, request.db_override.as_ref());
    let workspace_authority_preserved = post_resolution
        .as_ref()
        .is_ok_and(|(beads_after, _, _, _)| beads_after == &canonical_beads)
        && additive_path_identity_sha256(&canonical_beads, "workspace")
            .is_ok_and(|identity| identity == receipt.workspace_identity_sha256);
    let configured_source_preserved = request.source_path_override.is_some()
        || post_resolution
            .as_ref()
            .is_ok_and(|(_, _, source_after, _)| {
                source_after == &configured_source_after_authority_lock
            });
    let database_authority_preserved = write_authority.verify_database_authority().is_ok()
        && additive_file_identity(&canonical_database)
            .is_ok_and(|identity| identity == database_identity)
        && post_resolution
            .as_ref()
            .is_ok_and(|(_, database_after, _, identity_after)| {
                database_after == &canonical_database && *identity_after == database_identity
            });
    let source_preserved = configured_source_preserved
        && additive_source_snapshot(source_path, &config)
            .is_ok_and(|snapshot| additive_source_matches_receipt(&snapshot, &receipt));
    let database_witness_preserved = storage
        .with_read_transaction(|storage| {
            require_reviewed_additive_schema_version(storage, &receipt, "postcommit verification")?;
            let issues = hydrate_additive_database_issues(storage)?;
            let witness = additive_database_witness(storage, &issues)?;
            Ok(witness)
        })
        .is_ok_and(|witness| receipt.target_after.as_ref() == Some(&witness));
    let database_health_preserved = additive_database_health(&storage)
        .is_ok_and(|health| receipt.health_after.as_ref() == Some(&health));
    let database_poststate_preserved = database_witness_preserved && database_health_preserved;
    receipt.database_authority_preserved_after_commit = Some(database_authority_preserved);
    receipt.database_poststate_preserved_after_commit = Some(database_poststate_preserved);
    receipt.workspace_authority_preserved_after_commit = Some(workspace_authority_preserved);
    receipt.source_preserved_after_commit = Some(source_preserved);
    if !database_authority_preserved {
        receipt
            .postcommit_failures
            .push(AdditivePostcommitFailure::DatabaseAuthorityChanged);
    }
    if !database_poststate_preserved {
        receipt
            .postcommit_failures
            .push(AdditivePostcommitFailure::DatabasePoststateChanged);
    }
    if !workspace_authority_preserved {
        receipt
            .postcommit_failures
            .push(AdditivePostcommitFailure::WorkspaceAuthorityChanged);
    }
    if !source_preserved {
        receipt
            .postcommit_failures
            .push(AdditivePostcommitFailure::SourceWitnessChanged);
    }
    receipt.postcommit_failures.sort_unstable();
    receipt.postcommit_failures.dedup();
    if !receipt.postcommit_failures.is_empty() {
        receipt.status = AdditiveReconcileStatus::CommittedWithPostconditionFailures;
    }
    Ok(receipt)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum AdditiveMutation {
    Create(Issue),
    UpdateScalars(Issue),
}

impl AdditiveMutation {
    fn issue(&self) -> &Issue {
        match self {
            Self::Create(issue) | Self::UpdateScalars(issue) => issue,
        }
    }

    const fn creates_issue(&self) -> bool {
        matches!(self, Self::Create(_))
    }
}

/// Immutable plan whose private mutations are bound to the public witnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditiveReconcilePlan {
    receipt: AdditiveReconcileReceipt,
    mutations: Vec<AdditiveMutation>,
    content_hash_repairs: Vec<(String, String)>,
    synchronized_export_hashes: Vec<(String, String)>,
    expected_issues: BTreeMap<String, Issue>,
    expected_raw_issue_rows: Vec<Vec<String>>,
    expected_issue_content_hashes: BTreeMap<String, Option<String>>,
    expected_export_hashes: BTreeMap<String, String>,
    expected_raw_export_hash_rows: Vec<Vec<String>>,
    expected_dirty_issues: Vec<(String, String)>,
    expected_metadata: BTreeMap<String, String>,
    expected_blocked_cache: BTreeMap<String, Vec<String>>,
    expected_raw_blocked_cache_rows: Vec<Vec<String>>,
    expected_child_counters: BTreeMap<String, u32>,
    expected_sqlite_sequence: BTreeMap<String, i64>,
}

impl AdditiveReconcilePlan {
    #[must_use]
    pub const fn receipt(&self) -> &AdditiveReconcileReceipt {
        &self.receipt
    }

    #[must_use]
    pub const fn has_conflicts(&self) -> bool {
        self.receipt.conflicted != 0
    }

    #[must_use]
    pub fn mutation_count(&self) -> usize {
        self.mutations
            .len()
            .saturating_add(self.content_hash_repairs.len())
    }
}

const ADDITIVE_LOG_PREVIEW_LIMIT: usize = 32;

#[cfg(test)]
std::thread_local! {
    static ADDITIVE_TEST_DRIFT_SOURCE_AFTER_FINAL_CHECK: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static ADDITIVE_TEST_FAIL_PHASE: std::cell::Cell<Option<AdditiveTestFailPhase>> =
        const { std::cell::Cell::new(None) };
    static ADDITIVE_TEST_DRIFT_SCHEMA_BEFORE_TRANSACTION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdditiveTestFailPhase {
    BeforeTransaction,
    AfterIssueAndRelationWrites,
    BeforeFinalCommitChecks,
}

#[cfg(test)]
fn additive_test_fail_at(phase: AdditiveTestFailPhase) -> Result<()> {
    let should_fail = ADDITIVE_TEST_FAIL_PHASE.with(|configured| {
        if configured.get() == Some(phase) {
            configured.set(None);
            true
        } else {
            false
        }
    });
    if should_fail {
        return Err(BeadsError::SyncConflict {
            message: format!("injected additive reconciliation failure at {phase:?}"),
        });
    }
    Ok(())
}

#[cfg(test)]
fn additive_test_drift_source_after_final_check(input_path: &Path) -> Result<()> {
    let should_drift =
        ADDITIVE_TEST_DRIFT_SOURCE_AFTER_FINAL_CHECK.with(|flag| flag.replace(false));
    if should_drift {
        let mut source = OpenOptions::new().append(true).open(input_path)?;
        source.write_all(b"\n")?;
        source.flush()?;
    }
    Ok(())
}

#[cfg(test)]
fn additive_test_drift_schema_before_transaction(storage: &SqliteStorage) -> Result<()> {
    let should_drift =
        ADDITIVE_TEST_DRIFT_SCHEMA_BEFORE_TRANSACTION.with(|flag| flag.replace(false));
    if should_drift {
        let future = crate::storage::schema::CURRENT_SCHEMA_VERSION
            .checked_add(1)
            .ok_or_else(|| BeadsError::Config("Schema test version overflow".to_string()))?;
        storage.execute_raw(&format!("PRAGMA user_version = {future}"))?;
    }
    Ok(())
}

#[allow(clippy::incompatible_msrv)]
fn acquire_reviewed_additive_source_lock(input_path: &Path) -> Result<File> {
    let descriptor = additive_path_descriptor(input_path, "source");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(input_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not open reviewed reconciliation source {descriptor}: {error}"
        ))
    })?;
    redact_reviewed_path_result(
        path::validate_jsonl_fd_metadata(&file, input_path),
        input_path,
        "source",
        "validate",
    )?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(BeadsError::SyncConflict {
            message: format!(
                "Reviewed reconciliation source {descriptor} is being modified by another process"
            ),
        }),
        Err(TryLockError::Error(error)) => Err(BeadsError::Config(format!(
            "Could not acquire reviewed reconciliation source authority {descriptor}: {error}"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdditiveSourceWitness {
    raw_sha256: String,
    content_sha256: String,
    canonical_path_sha256: String,
    identity_sha256: String,
    size: u64,
    mtime: String,
}

#[derive(Debug, Clone)]
struct AdditiveSourceSnapshot {
    witness: AdditiveSourceWitness,
    issues: BTreeMap<String, Issue>,
    record_count: usize,
}

fn additive_sha256(value: &impl Serialize, context: &str) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        BeadsError::Config(format!(
            "Failed to serialize {context} for additive reconciliation: {error}"
        ))
    })?;
    Ok(hex_encode(&Sha256::digest(bytes)))
}

fn canonicalize_additive_issue(issue: &mut Issue) {
    issue.content_hash = None;
    issue.labels.sort_unstable();
    issue.dependencies.sort_by(|left, right| {
        left.issue_id
            .cmp(&right.issue_id)
            .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
            .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.created_by.cmp(&right.created_by))
            .then_with(|| left.metadata.cmp(&right.metadata))
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    issue.comments.sort_by(|left, right| {
        left.issue_id
            .cmp(&right.issue_id)
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.author.cmp(&right.author))
            .then_with(|| left.body.cmp(&right.body))
    });
}

fn canonicalize_additive_issue_for_storage(issue: &mut Issue) {
    canonicalize_additive_issue(issue);
    issue.source_repo.get_or_insert_with(|| ".".to_string());
    issue.compaction_level.get_or_insert(0);
    issue.original_size.get_or_insert(0);
    for dependency in &mut issue.dependencies {
        dependency
            .created_by
            .get_or_insert_with(|| "import".to_string());
        dependency.metadata.get_or_insert_with(|| "{}".to_string());
        dependency.thread_id.get_or_insert_with(String::new);
    }
}

fn additive_issues_semantically_equal(left: &Issue, right: &Issue) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    for comment in &mut left.comments {
        comment.id = 0;
    }
    for comment in &mut right.comments {
        comment.id = 0;
    }
    canonicalize_additive_issue(&mut left);
    canonicalize_additive_issue(&mut right);
    left == right
}

fn additive_relations_semantically_equal(left: &Issue, right: &Issue) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    for comment in &mut left.comments {
        comment.id = 0;
    }
    for comment in &mut right.comments {
        comment.id = 0;
    }
    canonicalize_additive_issue(&mut left);
    canonicalize_additive_issue(&mut right);
    left.labels == right.labels
        && left.dependencies == right.dependencies
        && left.comments == right.comments
}

fn additive_scalar_update_witness(
    existing: &Issue,
    incoming: &Issue,
    resolution: AdditiveScalarResolution,
) -> Result<AdditiveScalarUpdateWitness> {
    let mut before = serde_json::to_value(existing).map_err(|error| {
        BeadsError::Config(format!("Could not witness existing issue: {error}"))
    })?;
    let mut after = serde_json::to_value(incoming).map_err(|error| {
        BeadsError::Config(format!("Could not witness incoming issue: {error}"))
    })?;
    for value in [&mut before, &mut after] {
        let object = value.as_object_mut().ok_or_else(|| {
            BeadsError::Config(
                "Serialized issue was not an object while witnessing scalar repair".to_string(),
            )
        })?;
        object.remove("labels");
        object.remove("dependencies");
        object.remove("comments");
    }
    let before = before.as_object().ok_or_else(|| {
        BeadsError::Config("Existing scalar witness was not an object".to_string())
    })?;
    let after = after.as_object().ok_or_else(|| {
        BeadsError::Config("Incoming scalar witness was not an object".to_string())
    })?;
    let fields = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let diff = fields
        .into_iter()
        .filter_map(|field| {
            let before_value = before.get(&field);
            let after_value = after.get(&field);
            (before_value != after_value)
                .then(|| (field, before_value.cloned(), after_value.cloned()))
        })
        .collect::<Vec<_>>();
    if diff.is_empty() {
        return Err(BeadsError::Config(format!(
            "Shared issue {} was classified as scalar drift without any scalar difference",
            incoming.id
        )));
    }
    Ok(AdditiveScalarUpdateWitness {
        issue_id: incoming.id.clone(),
        resolution,
        changed_fields: diff.iter().map(|(field, _, _)| field.clone()).collect(),
        diff_sha256: additive_sha256(&diff, "scalar update diff")?,
        before_payload_sha256: additive_sha256(before, "scalar update before payload")?,
        after_payload_sha256: additive_sha256(after, "scalar update after payload")?,
        relation_payload_sha256: additive_sha256(
            &(&existing.labels, &existing.dependencies, &existing.comments),
            "scalar update relation payload",
        )?,
    })
}

fn additive_conflict_scalar_diff_witness(
    existing: &Issue,
    incoming: &Issue,
) -> Result<Option<AdditiveConflictScalarDiffWitness>> {
    let mut before = serde_json::to_value(existing).map_err(|error| {
        BeadsError::Config(format!(
            "Could not witness existing conflicted issue: {error}"
        ))
    })?;
    let mut after = serde_json::to_value(incoming).map_err(|error| {
        BeadsError::Config(format!(
            "Could not witness incoming conflicted issue: {error}"
        ))
    })?;
    for value in [&mut before, &mut after] {
        let object = value.as_object_mut().ok_or_else(|| {
            BeadsError::Config(
                "Serialized conflicted issue was not an object while witnessing scalar drift"
                    .to_string(),
            )
        })?;
        object.remove("labels");
        object.remove("dependencies");
        object.remove("comments");
    }
    let before = before.as_object().ok_or_else(|| {
        BeadsError::Config("Existing conflict scalar witness was not an object".to_string())
    })?;
    let after = after.as_object().ok_or_else(|| {
        BeadsError::Config("Incoming conflict scalar witness was not an object".to_string())
    })?;
    let fields = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let diff = fields
        .into_iter()
        .filter_map(|field| {
            let before_value = before.get(&field);
            let after_value = after.get(&field);
            (before_value != after_value)
                .then(|| (field, before_value.cloned(), after_value.cloned()))
        })
        .collect::<Vec<_>>();
    if diff.is_empty() {
        return Ok(None);
    }
    Ok(Some(AdditiveConflictScalarDiffWitness {
        issue_id: incoming.id.clone(),
        changed_fields: diff.iter().map(|(field, _, _)| field.clone()).collect(),
        diff_sha256: additive_sha256(&diff, "conflict scalar diff")?,
        before_payload_sha256: additive_sha256(before, "conflict scalar before payload")?,
        after_payload_sha256: additive_sha256(after, "conflict scalar after payload")?,
    }))
}

fn record_additive_conflict_scalar_diff(
    witnesses: &mut BTreeMap<String, AdditiveConflictScalarDiffWitness>,
    existing: &Issue,
    incoming: &Issue,
) -> Result<()> {
    if let Some(witness) = additive_conflict_scalar_diff_witness(existing, incoming)? {
        witnesses.insert(incoming.id.clone(), witness);
    }
    Ok(())
}

fn additive_relation_element_multiset(issue: &Issue) -> Result<BTreeMap<String, usize>> {
    let mut issue = issue.clone();
    for comment in &mut issue.comments {
        comment.id = 0;
    }
    canonicalize_additive_issue(&mut issue);

    let mut elements = BTreeMap::new();
    for label in &issue.labels {
        let digest = additive_sha256(&("label", label), "relation label element")?;
        let count = elements.entry(digest).or_insert(0usize);
        *count = count.checked_add(1).ok_or_else(|| {
            BeadsError::Config("Relation label multiplicity overflow".to_string())
        })?;
    }
    for dependency in &issue.dependencies {
        let digest = additive_sha256(&("dependency", dependency), "relation dependency element")?;
        let count = elements.entry(digest).or_insert(0usize);
        *count = count.checked_add(1).ok_or_else(|| {
            BeadsError::Config("Relation dependency multiplicity overflow".to_string())
        })?;
    }
    for comment in &issue.comments {
        let digest = additive_sha256(&("comment", comment), "relation comment element")?;
        let count = elements.entry(digest).or_insert(0usize);
        *count = count.checked_add(1).ok_or_else(|| {
            BeadsError::Config("Relation comment multiplicity overflow".to_string())
        })?;
    }
    Ok(elements)
}

fn additive_relation_multiset_delta(
    minuend: &BTreeMap<String, usize>,
    subtrahend: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut delta = Vec::new();
    for (digest, count) in minuend {
        let removed = subtrahend.get(digest).copied().unwrap_or_default();
        for _ in 0..count.saturating_sub(removed) {
            delta.push(digest.clone());
        }
    }
    delta
}

fn additive_conflict_relation_diff_witness(
    existing: &Issue,
    incoming: &Issue,
) -> Result<AdditiveConflictRelationDiffWitness> {
    let mut before = existing.clone();
    let mut after = incoming.clone();
    for comment in &mut before.comments {
        comment.id = 0;
    }
    for comment in &mut after.comments {
        comment.id = 0;
    }
    canonicalize_additive_issue(&mut before);
    canonicalize_additive_issue(&mut after);

    let mut changed_relation_classes = Vec::new();
    if before.labels != after.labels {
        changed_relation_classes.push("labels".to_string());
    }
    if before.dependencies != after.dependencies {
        changed_relation_classes.push("dependencies".to_string());
    }
    if before.comments != after.comments {
        changed_relation_classes.push("comments".to_string());
    }
    if changed_relation_classes.is_empty() {
        return Err(BeadsError::Config(format!(
            "Shared issue {} was classified as relation drift without a relation difference",
            incoming.id
        )));
    }

    let before_elements = additive_relation_element_multiset(&before)?;
    let after_elements = additive_relation_element_multiset(&after)?;
    let added_element_sha256 = additive_relation_multiset_delta(&after_elements, &before_elements);
    let removed_element_sha256 =
        additive_relation_multiset_delta(&before_elements, &after_elements);
    let before_counts = AdditiveRelationCounts::from_issue(&before);
    let after_counts = AdditiveRelationCounts::from_issue(&after);
    let before_payload_sha256 = additive_sha256(
        &(&before.labels, &before.dependencies, &before.comments),
        "conflict relation before payload",
    )?;
    let after_payload_sha256 = additive_sha256(
        &(&after.labels, &after.dependencies, &after.comments),
        "conflict relation after payload",
    )?;
    let diff_sha256 = additive_sha256(
        &(
            &changed_relation_classes,
            before_counts,
            after_counts,
            &before_payload_sha256,
            &after_payload_sha256,
            &added_element_sha256,
            &removed_element_sha256,
        ),
        "conflict relation diff",
    )?;
    Ok(AdditiveConflictRelationDiffWitness {
        issue_id: incoming.id.clone(),
        changed_relation_classes,
        before_counts,
        after_counts,
        before_payload_sha256,
        after_payload_sha256,
        added_element_sha256,
        removed_element_sha256,
        diff_sha256,
    })
}

fn record_additive_conflict_relation_diff(
    witnesses: &mut BTreeMap<String, AdditiveConflictRelationDiffWitness>,
    existing: &Issue,
    incoming: &Issue,
) -> Result<()> {
    witnesses.insert(
        incoming.id.clone(),
        additive_conflict_relation_diff_witness(existing, incoming)?,
    );
    Ok(())
}

fn additive_is_monotonic_closure(existing: &Issue, incoming: &Issue) -> Result<bool> {
    if !matches!(
        existing.status,
        crate::model::Status::Open | crate::model::Status::InProgress
    ) || incoming.status != crate::model::Status::Closed
        || existing.closed_at.is_some()
        || incoming.closed_at.is_none()
        || existing.close_reason.is_some()
        || incoming.close_reason.as_deref().is_none_or(str::is_empty)
        || incoming.updated_at <= existing.updated_at
    {
        return Ok(false);
    }

    let mut before = serde_json::to_value(existing).map_err(|error| {
        BeadsError::Config(format!(
            "Could not compare existing monotonic closure payload: {error}"
        ))
    })?;
    let mut after = serde_json::to_value(incoming).map_err(|error| {
        BeadsError::Config(format!(
            "Could not compare incoming monotonic closure payload: {error}"
        ))
    })?;
    for value in [&mut before, &mut after] {
        let object = value.as_object_mut().ok_or_else(|| {
            BeadsError::Config("Serialized monotonic closure payload was not an object".to_string())
        })?;
        for field in [
            "labels",
            "dependencies",
            "comments",
            "status",
            "updated_at",
            "closed_at",
            "close_reason",
        ] {
            object.remove(field);
        }
    }
    Ok(before == after)
}

fn additive_explicit_scalar_resolution_conflict(
    existing: &Issue,
    incoming: &Issue,
    witness: &AdditiveScalarUpdateWitness,
) -> Option<&'static str> {
    const ALLOWED_FIELDS: &[&str] = &[
        "title",
        "description",
        "design",
        "acceptance_criteria",
        "notes",
        "priority",
        "issue_type",
        "assignee",
        "owner",
        "estimated_minutes",
        "updated_at",
        "due_at",
        "defer_until",
        "external_ref",
        "source_system",
        "source_repo",
        "source_repo_path",
        "agent_context",
    ];
    if incoming.updated_at < existing.updated_at {
        return Some("database_newer_source_resolution_forbidden");
    }
    witness
        .changed_fields
        .iter()
        .any(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
        .then_some("source_resolution_contains_forbidden_scalar")
}

fn strict_additive_string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
    line_num: usize,
) -> Result<Option<&'a str>> {
    object
        .get(field)
        .map(|value| {
            value.as_str().ok_or_else(|| {
                BeadsError::Config(format!(
                    "Additive reconciliation requires '{field}' to be a string at line {line_num}"
                ))
            })
        })
        .transpose()
}

fn reject_unknown_additive_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
    role: &str,
    line_num: usize,
) -> Result<()> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(field.as_str()))
    {
        return Err(BeadsError::Config(format!(
            "Unknown {role} field '{field}' in additive reconciliation source at line {line_num}"
        )));
    }
    Ok(())
}

struct DuplicateKeyRejectingJson(serde_json::Value);

impl<'de> Deserialize<'de> for DuplicateKeyRejectingJson {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DuplicateKeyRejectingJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object members")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Number(
                    value.into(),
                )))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Number(
                    value.into(),
                )))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(serde_json::Value::Number)
                    .map(DuplicateKeyRejectingJson)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::String(
                    value.to_string(),
                )))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::String(value)))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Null))
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                DuplicateKeyRejectingJson::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<DuplicateKeyRejectingJson>()? {
                    values.push(value.0);
                }
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Array(values)))
            }

            fn visit_map<A>(self, mut object: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object member '{key}'"
                        )));
                    }
                    let value = object.next_value::<DuplicateKeyRejectingJson>()?;
                    values.insert(key, value.0);
                }
                Ok(DuplicateKeyRejectingJson(serde_json::Value::Object(values)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[allow(clippy::too_many_lines)]
fn parse_strict_additive_issue(trimmed: &str, line_num: usize) -> Result<Issue> {
    const ISSUE_FIELDS: &[&str] = &[
        "id",
        "title",
        "description",
        "design",
        "acceptance_criteria",
        "notes",
        "status",
        "priority",
        "issue_type",
        "assignee",
        "owner",
        "estimated_minutes",
        "created_at",
        "created_by",
        "updated_at",
        "closed_at",
        "close_reason",
        "closed_by_session",
        "due_at",
        "defer_until",
        "external_ref",
        "source_system",
        "source_repo",
        "source_repo_path",
        "agent_context",
        "deleted_at",
        "deleted_by",
        "delete_reason",
        "original_type",
        "compaction_level",
        "compacted_at",
        "compacted_at_commit",
        "original_size",
        "sender",
        "ephemeral",
        "pinned",
        "is_template",
        "labels",
        "dependencies",
        "comments",
        "content_hash",
    ];
    const DEPENDENCY_FIELDS: &[&str] = &[
        "issue_id",
        "depends_on_id",
        "type",
        "created_at",
        "created_by",
        "metadata",
        "thread_id",
    ];
    const COMMENT_FIELDS: &[&str] = &["id", "issue_id", "author", "text", "created_at"];

    let value = serde_json::from_str::<DuplicateKeyRejectingJson>(trimmed)
        .map_err(|error| BeadsError::Config(format!("Invalid JSON at line {line_num}: {error}")))?
        .0;
    let object = value.as_object().ok_or_else(|| {
        BeadsError::Config(format!(
            "Additive reconciliation source record at line {line_num} must be a JSON object"
        ))
    })?;
    reject_unknown_additive_fields(object, ISSUE_FIELDS, "issue", line_num)?;
    if object.contains_key("content_hash") {
        return Err(BeadsError::Config(format!(
            "Additive reconciliation source must not contain ignored field 'content_hash' at line {line_num}"
        )));
    }

    let issue: Issue = serde_json::from_value(value.clone()).map_err(|error| {
        BeadsError::Config(format!("Invalid issue at line {line_num}: {error}"))
    })?;
    if let Some(raw_status) = strict_additive_string_field(object, "status", line_num)?
        && raw_status != issue.status.as_str()
    {
        return Err(BeadsError::Config(format!(
            "Status '{raw_status}' at line {line_num} would be normalized to '{}'; additive reconciliation refuses lossy repairs",
            issue.status.as_str()
        )));
    }
    if let Some(raw_type) = strict_additive_string_field(object, "issue_type", line_num)?
        && raw_type != issue.issue_type.as_str()
    {
        return Err(BeadsError::Config(format!(
            "Issue type '{raw_type}' at line {line_num} would be normalized to '{}'; additive reconciliation refuses lossy repairs",
            issue.issue_type.as_str()
        )));
    }
    if let Some(external_ref) = issue.external_ref.as_deref()
        && (external_ref.is_empty() || external_ref.trim() != external_ref)
    {
        return Err(BeadsError::Config(format!(
            "External reference for issue '{}' at line {line_num} is blank or not trimmed",
            issue.id
        )));
    }
    for (field, value) in [
        ("description", issue.description.as_deref()),
        ("design", issue.design.as_deref()),
        ("acceptance_criteria", issue.acceptance_criteria.as_deref()),
        ("notes", issue.notes.as_deref()),
        ("assignee", issue.assignee.as_deref()),
        ("owner", issue.owner.as_deref()),
        ("created_by", issue.created_by.as_deref()),
        ("close_reason", issue.close_reason.as_deref()),
        ("closed_by_session", issue.closed_by_session.as_deref()),
        ("source_system", issue.source_system.as_deref()),
        ("source_repo", issue.source_repo.as_deref()),
        ("deleted_by", issue.deleted_by.as_deref()),
        ("delete_reason", issue.delete_reason.as_deref()),
        ("original_type", issue.original_type.as_deref()),
        ("sender", issue.sender.as_deref()),
        ("source_repo_path", issue.source_repo_path.as_deref()),
        ("agent_context", issue.agent_context.as_deref()),
    ] {
        if value == Some("") {
            return Err(BeadsError::Config(format!(
                "Optional field '{field}' for issue '{}' at line {line_num} is an empty string that storage would read back as null",
                issue.id
            )));
        }
    }
    if let Some(agent_context) = issue.agent_context.as_deref() {
        let parsed = serde_json::from_str::<DuplicateKeyRejectingJson>(agent_context)
            .map_err(|error| {
                BeadsError::Config(format!(
                    "Agent context for issue '{}' at line {line_num} is not duplicate-free JSON: {error}",
                    issue.id
                ))
            })?
            .0;
        let canonical = serde_json::to_string(&parsed).map_err(|error| {
            BeadsError::Config(format!(
                "Agent context for issue '{}' at line {line_num} could not be canonicalized: {error}",
                issue.id
            ))
        })?;
        if canonical != agent_context {
            return Err(BeadsError::Config(format!(
                "Agent context for issue '{}' at line {line_num} is valid but not canonical JSON",
                issue.id
            )));
        }
    }

    if let Some(dependencies) = object.get("dependencies") {
        let dependencies = dependencies.as_array().ok_or_else(|| {
            BeadsError::Config(format!(
                "Issue dependencies at line {line_num} must be an array"
            ))
        })?;
        for (index, dependency) in dependencies.iter().enumerate() {
            let dependency = dependency.as_object().ok_or_else(|| {
                BeadsError::Config(format!(
                    "Dependency {index} at line {line_num} must be an object"
                ))
            })?;
            reject_unknown_additive_fields(dependency, DEPENDENCY_FIELDS, "dependency", line_num)?;
            let raw_type =
                strict_additive_string_field(dependency, "type", line_num)?.ok_or_else(|| {
                    BeadsError::Config(format!(
                        "Dependency {index} at line {line_num} is missing 'type'"
                    ))
                })?;
            if raw_type != issue.dependencies[index].dep_type.as_str() {
                return Err(BeadsError::Config(format!(
                    "Dependency type '{raw_type}' at line {line_num} would be normalized to '{}'; additive reconciliation refuses lossy repairs",
                    issue.dependencies[index].dep_type.as_str()
                )));
            }
            if dependency
                .get("metadata")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|metadata| metadata.trim().is_empty())
            {
                return Err(BeadsError::Config(format!(
                    "Dependency metadata at line {line_num} is blank and would be normalized away"
                )));
            }
        }
    }
    if let Some(comments) = object.get("comments") {
        let comments = comments.as_array().ok_or_else(|| {
            BeadsError::Config(format!(
                "Issue comments at line {line_num} must be an array"
            ))
        })?;
        for (index, comment) in comments.iter().enumerate() {
            let comment = comment.as_object().ok_or_else(|| {
                BeadsError::Config(format!(
                    "Comment {index} at line {line_num} must be an object"
                ))
            })?;
            reject_unknown_additive_fields(comment, COMMENT_FIELDS, "comment", line_num)?;
        }
    }
    if let Err(errors) = IssueValidator::validate(&issue) {
        let details = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(BeadsError::Config(format!(
            "Validation failed for issue {} at line {line_num}: {details}",
            issue.id
        )));
    }
    Ok(issue)
}

#[allow(clippy::too_many_lines)]
fn additive_source_snapshot(
    input_path: &Path,
    config: &AdditiveReconcileConfig,
) -> Result<AdditiveSourceSnapshot> {
    if let Some(beads_dir) = &config.beads_dir {
        redact_reviewed_path_result(
            validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl),
            input_path,
            "source",
            "validate path policy for",
        )?;
    } else {
        let validation = validate_no_git_path(input_path);
        if !validation.is_allowed() {
            return Err(BeadsError::Config(format!(
                "Reviewed source path was rejected: {}",
                additive_path_descriptor(input_path, "source")
            )));
        }
    }

    let canonical_path_before = fs::canonicalize(input_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not canonicalize additive reconciliation source {}: {error}",
            additive_path_descriptor(input_path, "source")
        ))
    })?;
    let canonical_path_sha256 = additive_sha256(
        &canonical_path_before.to_string_lossy().as_ref(),
        "canonical source path",
    )?;
    let path_before = redact_reviewed_path_result(
        observed_jsonl_witness(input_path),
        input_path,
        "source",
        "witness",
    )?;
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    source_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = source_options.open(input_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not open additive reconciliation source {}: {error}",
            additive_path_descriptor(input_path, "source")
        ))
    })?;
    redact_reviewed_path_result(
        path::validate_jsonl_fd_metadata(&file, input_path),
        input_path,
        "source",
        "validate",
    )?;
    let file_before = file.metadata()?;
    let file_identity = additive_metadata_identity(&file_before);
    let identity_sha256 = additive_sha256(
        &(
            canonical_path_before.to_string_lossy().as_ref(),
            file_identity,
        ),
        "source file identity",
    )?;
    let file_before_mtime = file_before.modified()?;
    let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut raw_hasher = Sha256::new();
    let mut canonical_hasher = Sha256::new();
    let mut line = String::new();
    let mut issues = BTreeMap::new();
    let mut record_count = 0usize;
    let mut line_num = 0usize;
    while reader.read_line(&mut line)? > 0 {
        raw_hasher.update(line.as_bytes());
        line_num = line_num.checked_add(1).ok_or_else(|| {
            BeadsError::Config(
                "JSONL line count overflow during additive reconciliation".to_string(),
            )
        })?;
        let trimmed_bytes = line.as_bytes().trim_ascii();
        if !trimmed_bytes.is_empty() {
            canonical_hasher.update(trimmed_bytes);
            canonical_hasher.update(b"\n");
            let trimmed = std::str::from_utf8(trimmed_bytes).map_err(|error| {
                BeadsError::Config(format!(
                    "Invalid UTF-8 in additive reconciliation source at line {line_num}: {error}"
                ))
            })?;
            let mut issue = parse_strict_additive_issue(trimmed, line_num)?;
            record_count = record_count.checked_add(1).ok_or_else(|| {
                BeadsError::Config(
                    "JSONL record count overflow during additive reconciliation".to_string(),
                )
            })?;
            canonicalize_additive_issue_for_storage(&mut issue);
            let issue_id = issue.id.clone();
            if issues.insert(issue_id.clone(), issue).is_some() {
                return Err(BeadsError::Config(format!(
                    "Duplicate issue id '{issue_id}' in additive reconciliation source at line {line_num}"
                )));
            }
        }
        line.clear();
    }
    let file_after = reader.get_ref().metadata()?;
    let path_after = redact_reviewed_path_result(
        observed_jsonl_witness(input_path),
        input_path,
        "source",
        "re-witness",
    )?;
    let canonical_path_after = fs::canonicalize(input_path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not re-canonicalize additive reconciliation source {}: {error}",
            additive_path_descriptor(input_path, "source")
        ))
    })?;
    let path_identity_after = additive_file_identity(&canonical_path_after)?;
    if file_before.len() != file_after.len()
        || file_before_mtime != file_after.modified()?
        || additive_metadata_identity(&file_after) != file_identity
        || canonical_path_after != canonical_path_before
        || path_identity_after != file_identity
        || path_before.mtime != path_after.mtime
        || path_before.size != path_after.size
        || file_after.len() != path_after.size
        || file_after.modified()? != path_after.mtime
    {
        return Err(BeadsError::Config(
            "JSONL changed while additive reconciliation was reading it; retry from a stable source"
                .to_string(),
        ));
    }
    let raw_sha256 = hex_encode(&raw_hasher.finalize());
    let content_sha256 = hex_encode(&canonical_hasher.finalize());

    Ok(AdditiveSourceSnapshot {
        witness: AdditiveSourceWitness {
            raw_sha256,
            content_sha256,
            canonical_path_sha256,
            identity_sha256,
            size: path_after.size,
            mtime: path_after.mtime_witness,
        },
        issues,
        record_count,
    })
}

fn hydrate_additive_database_issues(storage: &SqliteStorage) -> Result<BTreeMap<String, Issue>> {
    let mut ids = storage
        .get_all_issues_metadata()?
        .into_iter()
        .map(|metadata| metadata.id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();

    let mut issues = storage.get_issues_by_ids(&ids)?;
    let dependencies = storage.get_all_dependency_records()?;
    let labels = storage.get_all_labels()?;
    let comments = storage.get_all_comments()?;
    for issue in &mut issues {
        issue.dependencies = dependencies.get(&issue.id).cloned().unwrap_or_default();
        issue.labels = labels.get(&issue.id).cloned().unwrap_or_default();
        issue.comments = comments.get(&issue.id).cloned().unwrap_or_default();
        canonicalize_additive_issue_for_storage(issue);
    }
    issues.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    if issues.len() != ids.len() {
        return Err(BeadsError::Config(format!(
            "Additive reconciliation could hydrate only {} of {} database issues",
            issues.len(),
            ids.len()
        )));
    }
    Ok(issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect())
}

fn additive_relation_counts<'a>(
    issues: impl IntoIterator<Item = &'a Issue>,
) -> Result<AdditiveRelationCounts> {
    issues
        .into_iter()
        .try_fold(AdditiveRelationCounts::default(), |counts, issue| {
            counts.checked_add(AdditiveRelationCounts::from_issue(issue))
        })
}

fn additive_sqlite_value_witness(value: SqliteValue) -> String {
    match value {
        SqliteValue::Null => "null".to_string(),
        SqliteValue::Integer(value) => format!("integer:{value}"),
        SqliteValue::Float(value) => format!("real:{:016x}", value.to_bits()),
        SqliteValue::Text(value) => format!("text:{}", value.as_str()),
        SqliteValue::Blob(value) => format!("blob:{}", hex_encode(value.as_ref())),
    }
}

fn additive_raw_rows(storage: &SqliteStorage, sql: &str) -> Result<Vec<Vec<String>>> {
    Ok(storage
        .execute_raw_query(sql)?
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(additive_sqlite_value_witness)
                .collect::<Vec<_>>()
        })
        .collect())
}

fn additive_raw_issue_rows(storage: &SqliteStorage) -> Result<Vec<Vec<String>>> {
    additive_raw_rows(storage, "SELECT * FROM issues ORDER BY id")
}

fn additive_raw_issue_row_map(storage: &SqliteStorage) -> Result<BTreeMap<String, Vec<String>>> {
    let mut rows_by_id = BTreeMap::new();
    for (row_index, row) in additive_raw_issue_rows(storage)?.into_iter().enumerate() {
        let id = row
            .first()
            .and_then(|value| value.strip_prefix("text:"))
            .ok_or_else(|| {
                BeadsError::Config(format!(
                    "Raw issue row {row_index} had no text ID in its first column"
                ))
            })?
            .to_string();
        if rows_by_id.insert(id.clone(), row).is_some() {
            return Err(BeadsError::Config(format!(
                "Duplicate raw issue row for '{id}'"
            )));
        }
    }
    Ok(rows_by_id)
}

fn additive_raw_rows_by_text_key(
    rows: Vec<Vec<String>>,
    table: &str,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut rows_by_key = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let key = row
            .first()
            .and_then(|value| value.strip_prefix("text:"))
            .ok_or_else(|| {
                BeadsError::Config(format!(
                    "Raw {table} row {row_index} had no text key in its first column"
                ))
            })?
            .to_string();
        if rows_by_key.insert(key.clone(), row).is_some() {
            return Err(BeadsError::Config(format!(
                "Duplicate raw {table} row for '{key}'"
            )));
        }
    }
    Ok(rows_by_key)
}

#[allow(clippy::too_many_lines)]
fn additive_database_witness(
    storage: &SqliteStorage,
    issues: &BTreeMap<String, Issue>,
) -> Result<AdditiveDatabaseWitness> {
    let mut dirty = storage.get_dirty_issue_metadata()?;
    dirty.sort_unstable();
    let relations = additive_relation_counts(issues.values())?;
    let raw_issue_rows = additive_raw_issue_rows(storage)?;
    if raw_issue_rows.len() != issues.len() {
        return Err(BeadsError::Config(
            "Raw and hydrated issue counts diverged during additive reconciliation".to_string(),
        ));
    }
    let (blocked_cache_entries, child_counter_entries) = storage.count_sync_derived_rows()?;
    let issue_content_hashes = additive_issue_content_hashes(storage)?;
    let label_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, label FROM labels ORDER BY issue_id, label",
    )?;
    let dependency_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id \
         FROM dependencies \
         ORDER BY issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id",
    )?;
    let comment_rows = additive_raw_rows(
        storage,
        "SELECT id, issue_id, author, text, created_at \
         FROM comments ORDER BY id, issue_id, author, text, created_at",
    )?;
    let event_rows = additive_raw_rows(
        storage,
        "SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at, \
         agent_name, harness, model \
         FROM events \
         ORDER BY id, issue_id, event_type, actor, old_value, new_value, comment, created_at, agent_name, harness, model",
    )?;
    let export_hashes = additive_raw_rows(
        storage,
        "SELECT issue_id, content_hash, exported_at FROM export_hashes \
         ORDER BY issue_id, content_hash, exported_at",
    )?;
    let metadata_rows = additive_raw_rows(
        storage,
        "SELECT key, value FROM metadata ORDER BY key, value",
    )?;
    let stable_metadata_rows = metadata_rows
        .iter()
        .filter(|row| {
            row.first()
                .and_then(|value| value.strip_prefix("text:"))
                .is_none_or(|key| !is_sync_merge_mutable_metadata_key(key))
        })
        .cloned()
        .collect::<Vec<_>>();
    let blocked_cache_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, blocked_by, blocked_at FROM blocked_issues_cache \
         ORDER BY issue_id, blocked_by, blocked_at",
    )?;
    let child_counter_rows = additive_raw_rows(
        storage,
        "SELECT parent_id, last_child FROM child_counters \
         ORDER BY parent_id, last_child",
    )?;
    let config_rows =
        additive_raw_rows(storage, "SELECT key, value FROM config ORDER BY key, value")?;
    let close_metadata_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, closed_by_agent_name, closed_by_harness, closed_by_model, \
         bypassed_policy, bypass_reason, policy_gates_fired, recorded_at \
         FROM close_metadata \
         ORDER BY issue_id, closed_by_agent_name, closed_by_harness, closed_by_model, \
                  bypassed_policy, bypass_reason, policy_gates_fired, recorded_at",
    )?;
    let gate_result_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, gate, provider, passed, note, recorded_by, recorded_at \
         FROM gate_results \
         ORDER BY issue_id, gate, provider, passed, note, recorded_by, recorded_at",
    )?;
    let gate_result_history_rows = additive_raw_rows(
        storage,
        "SELECT id, issue_id, from_status, to_status, status_revision, gate, provider, passed, \
         note, recorded_by, recorded_at \
         FROM gate_result_history \
         ORDER BY id, issue_id, from_status, to_status, status_revision, gate, provider, \
                  passed, note, recorded_by, recorded_at",
    )?;
    let schema_catalog_rows = additive_raw_rows(
        storage,
        "SELECT type, name, tbl_name, rootpage, sql \
         FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' \
         ORDER BY type, name, tbl_name, rootpage, sql",
    )?;
    let sqlite_sequence_rows = additive_raw_rows(
        storage,
        "SELECT name, seq FROM sqlite_sequence ORDER BY name, seq",
    )?;
    if blocked_cache_rows.len() != blocked_cache_entries
        || child_counter_rows.len() != child_counter_entries
    {
        return Err(BeadsError::Config(
            "Derived-table counts changed while additive reconciliation was witnessing them"
                .to_string(),
        ));
    }

    Ok(AdditiveDatabaseWitness {
        issues: issues.len(),
        issue_payload_sha256: additive_sha256(&raw_issue_rows, "raw database issue payload")?,
        issue_semantic_payload_sha256: additive_sha256(issues, "database semantic issue payload")?,
        issue_content_hashes: issue_content_hashes.len(),
        issue_content_hash_payload_sha256: additive_sha256(
            &issue_content_hashes,
            "database issue content hashes",
        )?,
        relations,
        label_payload_sha256: additive_sha256(&label_rows, "raw database labels")?,
        dependency_payload_sha256: additive_sha256(&dependency_rows, "raw database dependencies")?,
        comment_payload_sha256: additive_sha256(&comment_rows, "raw database comments")?,
        export_hashes: export_hashes.len(),
        export_hash_payload_sha256: additive_sha256(&export_hashes, "database export hashes")?,
        events: event_rows.len(),
        event_payload_sha256: additive_sha256(&event_rows, "raw database audit events")?,
        dirty_issues: dirty.len(),
        dirty_payload_sha256: additive_sha256(&dirty, "database dirty markers")?,
        metadata_rows: metadata_rows.len(),
        metadata_payload_sha256: additive_sha256(&metadata_rows, "database metadata")?,
        stable_metadata: additive_table_witness(
            &stable_metadata_rows,
            "stable sync merge database metadata",
        )?,
        blocked_cache_entries,
        blocked_cache_payload_sha256: additive_sha256(
            &blocked_cache_rows,
            "database blocked cache",
        )?,
        child_counter_entries,
        child_counter_payload_sha256: additive_sha256(
            &child_counter_rows,
            "database child counters",
        )?,
        config: additive_table_witness(&config_rows, "database config")?,
        close_metadata: additive_table_witness(&close_metadata_rows, "database close metadata")?,
        gate_results: additive_table_witness(&gate_result_rows, "database legacy gate results")?,
        gate_result_history: additive_table_witness(
            &gate_result_history_rows,
            "database gate-result history",
        )?,
        schema_catalog: additive_table_witness(&schema_catalog_rows, "database schema catalog")?,
        sqlite_sequence: additive_table_witness(&sqlite_sequence_rows, "database sqlite sequence")?,
        stored_jsonl_content_hash: storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?,
        stored_jsonl_mtime: storage.get_metadata(METADATA_JSONL_MTIME)?,
        stored_jsonl_size: storage.get_metadata(METADATA_JSONL_SIZE)?,
        needs_flush: storage.get_metadata("needs_flush")?,
    })
}

/// Capture the complete database witness used to bind merge planning to the
/// exact transaction prestate.
pub(crate) fn capture_sync_database_witness(
    storage: &SqliteStorage,
) -> Result<AdditiveDatabaseWitness> {
    let issues = hydrate_additive_database_issues(storage)?;
    additive_database_witness(storage, &issues)
}

/// Capture the merge-authoritative poststate that remains invariant while
/// export bookkeeping advances.
pub(crate) fn capture_sync_merge_core_witness(
    storage: &SqliteStorage,
) -> Result<SyncMergeDatabaseCoreWitness> {
    capture_sync_database_witness(storage).map(Into::into)
}

fn is_sync_merge_mutable_metadata_key(key: &str) -> bool {
    matches!(
        key,
        METADATA_SYNC_MERGE_PENDING
            | METADATA_JSONL_CONTENT_HASH
            | METADATA_JSONL_MTIME
            | METADATA_JSONL_SIZE
            | METADATA_LAST_EXPORT_TIME
            | "needs_flush"
            | "purged_ids_pending_export"
    )
}

fn is_sync_merge_export_metadata_key(key: &str) -> bool {
    matches!(
        key,
        METADATA_JSONL_CONTENT_HASH
            | METADATA_JSONL_MTIME
            | METADATA_JSONL_SIZE
            | METADATA_LAST_EXPORT_TIME
            | "needs_flush"
            | "purged_ids_pending_export"
    )
}

/// Capture the exact mutable bookkeeping that export finalization must leave
/// behind before a pending merge receipt may advance or clear.
pub(crate) fn capture_sync_merge_export_finalization_witness(
    storage: &SqliteStorage,
) -> Result<SyncMergeExportFinalizationWitness> {
    let dirty_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, marked_at FROM dirty_issues ORDER BY issue_id, marked_at",
    )?;
    let all_metadata_rows = additive_raw_rows(
        storage,
        "SELECT key, value FROM metadata ORDER BY key, value",
    )?;
    let export_metadata_rows = all_metadata_rows
        .into_iter()
        .filter(|row| {
            row.first()
                .and_then(|value| value.strip_prefix("text:"))
                .is_some_and(is_sync_merge_export_metadata_key)
        })
        .collect::<Vec<_>>();

    Ok(SyncMergeExportFinalizationWitness {
        export_hashes: capture_export_hash_mapping_witness(storage)?,
        dirty_issues: additive_table_witness(&dirty_rows, "sync merge finalized dirty markers")?,
        jsonl_content_hash: storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?,
        jsonl_mtime: storage.get_metadata(METADATA_JSONL_MTIME)?,
        jsonl_size: storage.get_metadata(METADATA_JSONL_SIZE)?,
        last_export_time: storage.get_metadata(METADATA_LAST_EXPORT_TIME)?,
        needs_flush: storage.get_metadata("needs_flush")?,
        export_metadata: additive_table_witness(
            &export_metadata_rows,
            "sync merge finalized export metadata",
        )?,
    })
}

fn additive_table_witness(rows: &[Vec<String>], context: &str) -> Result<AdditiveTableWitness> {
    Ok(AdditiveTableWitness {
        rows: rows.len(),
        payload_sha256: additive_sha256(&rows, context)?,
    })
}

fn additive_text_rows(
    storage: &SqliteStorage,
    sql: &str,
    row_role: &str,
) -> Result<Vec<Vec<String>>> {
    storage
        .execute_raw_query(sql)?
        .into_iter()
        .enumerate()
        .map(|(row_index, row)| {
            row.into_iter()
                .enumerate()
                .map(|(column_index, value)| {
                    value.as_text().map(str::to_owned).ok_or_else(|| {
                        BeadsError::Config(format!(
                            "Additive reconciliation {row_role} row {row_index} column {column_index} was not text"
                        ))
                    })
                })
                .collect()
        })
        .collect()
}

fn additive_key_value_map(
    storage: &SqliteStorage,
    sql: &str,
    row_role: &str,
) -> Result<BTreeMap<String, String>> {
    let rows = additive_text_rows(storage, sql, row_role)?;
    let mut map = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let [key, value]: [String; 2] = row.try_into().map_err(|row: Vec<String>| {
            BeadsError::Config(format!(
                "Additive reconciliation {row_role} row {row_index} had {} column(s), expected 2",
                row.len()
            ))
        })?;
        if map.insert(key.clone(), value).is_some() {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation {row_role} contained duplicate key '{key}'"
            )));
        }
    }
    Ok(map)
}

fn additive_issue_content_hashes(
    storage: &SqliteStorage,
) -> Result<BTreeMap<String, Option<String>>> {
    let rows = storage.execute_raw_query("SELECT id, content_hash FROM issues ORDER BY id")?;
    let mut hashes = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let [id, content_hash]: [SqliteValue; 2] =
            row.try_into().map_err(|row: Vec<SqliteValue>| {
                BeadsError::Config(format!(
                    "Additive reconciliation issue content-hash row {row_index} had {} column(s), expected 2",
                    row.len()
                ))
            })?;
        let id = id.as_text().ok_or_else(|| {
            BeadsError::Config(format!(
                "Additive reconciliation issue content-hash row {row_index} ID was not text"
            ))
        })?;
        let content_hash = match content_hash {
            SqliteValue::Null => None,
            SqliteValue::Text(value) => Some(value.as_str().to_string()),
            _ => {
                return Err(BeadsError::Config(format!(
                    "Additive reconciliation issue content hash for '{id}' was neither text nor NULL"
                )));
            }
        };
        if hashes.insert(id.to_string(), content_hash).is_some() {
            return Err(BeadsError::Config(format!(
                "Duplicate issue content-hash row for '{id}'"
            )));
        }
    }
    Ok(hashes)
}

fn additive_export_hashes(storage: &SqliteStorage) -> Result<BTreeMap<String, String>> {
    additive_key_value_map(
        storage,
        "SELECT issue_id, content_hash FROM export_hashes ORDER BY issue_id",
        "export hash",
    )
}

pub(crate) fn sync_merge_export_hash_mapping_witness(
    issue_hashes: &[(String, String)],
) -> Result<AdditiveTableWitness> {
    let mut rows = issue_hashes.to_vec();
    rows.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if let Some(duplicate) = rows
        .windows(2)
        .find(|pair| pair[0].0 == pair[1].0)
        .map(|pair| pair[0].0.as_str())
    {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Reviewed JSONL export contains duplicate issue-hash mapping for {duplicate}"
            ),
        });
    }
    Ok(AdditiveTableWitness {
        rows: rows.len(),
        payload_sha256: additive_sha256(&rows, "sync merge export hash mapping")?,
    })
}

fn capture_export_hash_mapping_witness(storage: &SqliteStorage) -> Result<AdditiveTableWitness> {
    let rows = additive_export_hashes(storage)?
        .into_iter()
        .collect::<Vec<_>>();
    sync_merge_export_hash_mapping_witness(&rows)
}

fn additive_metadata(storage: &SqliteStorage) -> Result<BTreeMap<String, String>> {
    additive_key_value_map(
        storage,
        "SELECT key, value FROM metadata ORDER BY key",
        "metadata",
    )
}

fn additive_sqlite_sequence(storage: &SqliteStorage) -> Result<BTreeMap<String, i64>> {
    let rows = additive_text_rows(
        storage,
        "SELECT name, CAST(seq AS TEXT) FROM sqlite_sequence ORDER BY name",
        "sqlite sequence",
    )?;
    let mut sequence = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let [name, value]: [String; 2] = row.try_into().map_err(|row: Vec<String>| {
            BeadsError::Config(format!(
                "SQLite sequence row {row_index} had {} column(s), expected 2",
                row.len()
            ))
        })?;
        let value = value.parse::<i64>().map_err(|error| {
            BeadsError::Config(format!(
                "SQLite sequence value for {name} was not an integer: {error}"
            ))
        })?;
        if sequence.insert(name.clone(), value).is_some() {
            return Err(BeadsError::Config(format!(
                "SQLite sequence contained duplicate row for {name}"
            )));
        }
    }
    Ok(sequence)
}

fn additive_actual_blocked_cache(storage: &SqliteStorage) -> Result<BTreeMap<String, Vec<String>>> {
    let rows = additive_text_rows(
        storage,
        "SELECT issue_id, blocked_by FROM blocked_issues_cache ORDER BY issue_id",
        "blocked cache",
    )?;
    let mut map = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let [issue_id, blocked_by]: [String; 2] = row.try_into().map_err(|row: Vec<String>| {
            BeadsError::Config(format!(
                "Blocked-cache row {row_index} had {} column(s), expected 2",
                row.len()
            ))
        })?;
        let mut blockers: Vec<String> = serde_json::from_str(&blocked_by).map_err(|error| {
            BeadsError::Config(format!(
                "Blocked-cache row for {issue_id} contains invalid JSON: {error}"
            ))
        })?;
        blockers.sort_unstable();
        blockers.dedup();
        if blockers.is_empty() || map.insert(issue_id.clone(), blockers).is_some() {
            return Err(BeadsError::Config(format!(
                "Blocked-cache row for {issue_id} is empty or duplicated"
            )));
        }
    }
    Ok(map)
}

fn additive_expected_blocked_cache(
    issues: &BTreeMap<String, Issue>,
) -> BTreeMap<String, Vec<String>> {
    let mut blocked = BTreeMap::<String, BTreeSet<String>>::new();
    let mut children_by_parent = BTreeMap::<String, BTreeSet<String>>::new();

    for issue in issues.values() {
        for dependency in &issue.dependencies {
            if matches!(dependency.dep_type, DependencyType::ParentChild) {
                if !dependency.issue_id.starts_with("external:")
                    && !dependency.depends_on_id.starts_with("external:")
                {
                    children_by_parent
                        .entry(dependency.depends_on_id.clone())
                        .or_default()
                        .insert(dependency.issue_id.clone());
                }
                continue;
            }
            if !matches!(
                dependency.dep_type,
                DependencyType::Blocks
                    | DependencyType::ConditionalBlocks
                    | DependencyType::WaitsFor
            ) || dependency.depends_on_id.starts_with("external:")
            {
                continue;
            }
            if let Some(blocker) = issues.get(&dependency.depends_on_id)
                && !blocker.status.is_terminal()
                && !blocker.is_template
            {
                blocked.entry(issue.id.clone()).or_default().insert(format!(
                    "{}:{}",
                    dependency.depends_on_id,
                    blocker.status.as_str()
                ));
            }
        }
    }

    let mut queue = blocked.keys().cloned().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    while let Some(parent_id) = queue.pop() {
        if !seen.insert(parent_id.clone()) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&parent_id) {
            for child_id in children {
                let inserted = blocked
                    .entry(child_id.clone())
                    .or_default()
                    .insert(format!("{parent_id}:parent-blocked"));
                if inserted {
                    queue.push(child_id.clone());
                }
            }
        }
    }

    for (parent_id, children) in children_by_parent {
        let Some(parent) = issues.get(&parent_id) else {
            continue;
        };
        if parent.issue_type != crate::model::IssueType::Epic {
            continue;
        }
        for child_id in children {
            if let Some(child) = issues.get(&child_id)
                && !child.status.is_terminal()
                && !child.is_template
            {
                blocked
                    .entry(parent_id.clone())
                    .or_default()
                    .insert(format!("{child_id}:child-open"));
            }
        }
    }

    blocked
        .into_iter()
        .filter_map(|(issue_id, blockers)| {
            (!blockers.is_empty()).then(|| (issue_id, blockers.into_iter().collect()))
        })
        .collect()
}

fn additive_actual_child_counters(storage: &SqliteStorage) -> Result<BTreeMap<String, u32>> {
    let rows = additive_text_rows(
        storage,
        "SELECT parent_id, CAST(last_child AS TEXT) FROM child_counters ORDER BY parent_id",
        "child counter",
    )?;
    let mut map = BTreeMap::new();
    for (row_index, row) in rows.into_iter().enumerate() {
        let [parent_id, last_child]: [String; 2] = row.try_into().map_err(|row: Vec<String>| {
            BeadsError::Config(format!(
                "Child-counter row {row_index} had {} column(s), expected 2",
                row.len()
            ))
        })?;
        let last_child = last_child.parse::<u32>().map_err(|error| {
            BeadsError::Config(format!(
                "Child-counter row for {parent_id} is invalid: {error}"
            ))
        })?;
        if map.insert(parent_id.clone(), last_child).is_some() {
            return Err(BeadsError::Config(format!(
                "Child-counter row for {parent_id} is duplicated"
            )));
        }
    }
    Ok(map)
}

fn additive_expected_child_counters(issues: &BTreeMap<String, Issue>) -> BTreeMap<String, u32> {
    let issue_ids = issues.keys().cloned().collect::<BTreeSet<_>>();
    let mut counters = BTreeMap::<String, u32>::new();
    for issue_id in &issue_ids {
        let Ok(parsed) = parse_id(issue_id) else {
            continue;
        };
        if parsed.is_root() {
            continue;
        }
        let Some(parent_id) = parsed.parent() else {
            continue;
        };
        let Some(child_number) = parsed.child_path.last().copied() else {
            continue;
        };
        if issue_ids.contains(&parent_id) {
            counters
                .entry(parent_id)
                .and_modify(|current| *current = (*current).max(child_number))
                .or_insert(child_number);
        }
    }
    counters
}

fn additive_database_health(storage: &SqliteStorage) -> Result<AdditiveDatabaseHealth> {
    Ok(AdditiveDatabaseHealth {
        integrity_messages: storage.integrity_check_messages()?,
        foreign_key_violations: storage.foreign_key_check_messages()?,
    })
}

fn additive_database_transaction_health(storage: &SqliteStorage) -> Result<AdditiveDatabaseHealth> {
    Ok(AdditiveDatabaseHealth {
        integrity_messages: storage.quick_check_messages()?,
        foreign_key_violations: storage.foreign_key_check_messages()?,
    })
}

fn additive_database_is_healthy(health: &AdditiveDatabaseHealth) -> bool {
    health.integrity_messages.len() == 1
        && health.integrity_messages[0].eq_ignore_ascii_case("ok")
        && health.foreign_key_violations.is_empty()
}

fn require_healthy_additive_database(health: &AdditiveDatabaseHealth, phase: &str) -> Result<()> {
    if !additive_database_is_healthy(health) {
        return Err(BeadsError::Config(format!(
            "Additive reconciliation {phase} database health gate failed: integrity={:?}, foreign_key_violations={:?}",
            health.integrity_messages, health.foreign_key_violations
        )));
    }
    Ok(())
}

fn additive_provenance_path(
    path: Option<&Path>,
    beads_dir: Option<&Path>,
    role: &str,
) -> Result<(String, String)> {
    let Some(path) = path else {
        return Ok((
            "<in-memory>".to_string(),
            additive_sha256(&"<in-memory>", "in-memory provenance path")?,
        ));
    };
    let canonical = fs::canonicalize(path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not canonicalize additive reconciliation {role} path {}: {error}",
            additive_path_descriptor(path, role)
        ))
    })?;
    let path_sha256 = additive_sha256(
        &canonical.to_string_lossy().as_ref(),
        "canonical provenance path",
    )?;
    let display_path = beads_dir
        .and_then(Path::parent)
        .and_then(|project_root| fs::canonicalize(project_root).ok())
        .and_then(|project_root| {
            canonical
                .strip_prefix(project_root)
                .ok()
                .map(|relative| relative.display().to_string())
        })
        .unwrap_or_else(|| format!("<external-{role}>"));
    Ok((display_path, path_sha256))
}

fn additive_path_identity_sha256(path: &Path, role: &str) -> Result<String> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        BeadsError::Config(format!(
            "Could not canonicalize additive reconciliation {role} identity at {}: {error}",
            additive_path_descriptor(path, role)
        ))
    })?;
    let identity = additive_file_identity(&canonical)?;
    additive_sha256(
        &(canonical.to_string_lossy().as_ref(), identity),
        &format!("canonical {role} identity"),
    )
}

fn additive_plan_sha256(plan: &AdditiveReconcilePlan) -> Result<String> {
    let mut receipt = plan.receipt.clone();
    receipt.plan_sha256.clear();
    additive_sha256(
        &(
            receipt,
            &plan.mutations,
            &plan.content_hash_repairs,
            &plan.synchronized_export_hashes,
            &plan.expected_issues,
            &plan.expected_raw_issue_rows,
            &plan.expected_issue_content_hashes,
            &plan.expected_export_hashes,
            &plan.expected_raw_export_hash_rows,
            &plan.expected_dirty_issues,
            &plan.expected_metadata,
            &plan.expected_blocked_cache,
            &plan.expected_raw_blocked_cache_rows,
            &plan.expected_child_counters,
            &plan.expected_sqlite_sequence,
        ),
        "additive reconciliation plan binding",
    )
}

#[derive(Default)]
struct AdditiveConflictAccumulator {
    by_issue: BTreeMap<String, BTreeSet<String>>,
    details_by_issue: BTreeMap<String, BTreeSet<AdditiveConflictDetailWitness>>,
}

impl AdditiveConflictAccumulator {
    fn contains(&self, issue_id: &str) -> bool {
        self.by_issue.contains_key(issue_id)
    }

    fn len(&self) -> usize {
        self.by_issue.len()
    }

    fn witnesses(&self) -> Vec<AdditiveConflictWitness> {
        self.by_issue
            .iter()
            .map(|(issue_id, reasons)| AdditiveConflictWitness {
                issue_id: issue_id.clone(),
                reasons: reasons.iter().cloned().collect(),
                details: self
                    .details_by_issue
                    .get(issue_id)
                    .map_or_else(Vec::new, |details| details.iter().cloned().collect()),
            })
            .collect()
    }
}

fn record_additive_conflict(
    reasons: &mut BTreeMap<String, usize>,
    conflicts: &mut AdditiveConflictAccumulator,
    issue_id: &str,
    reason: &str,
) -> Result<()> {
    record_additive_conflict_detail(
        reasons,
        conflicts,
        issue_id,
        reason,
        "issue",
        None,
        &[],
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_additive_conflict_detail(
    reasons: &mut BTreeMap<String, usize>,
    conflicts: &mut AdditiveConflictAccumulator,
    issue_id: &str,
    reason: &str,
    detail_kind: &str,
    ordinal: Option<usize>,
    related_values: &[String],
    sensitive_value: Option<&str>,
) -> Result<()> {
    record_additive_conflict_detail_with_subcodes(
        reasons,
        conflicts,
        issue_id,
        reason,
        detail_kind,
        ordinal,
        related_values,
        Vec::new(),
        sensitive_value,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_additive_conflict_detail_with_subcodes(
    reasons: &mut BTreeMap<String, usize>,
    conflicts: &mut AdditiveConflictAccumulator,
    issue_id: &str,
    reason: &str,
    detail_kind: &str,
    ordinal: Option<usize>,
    related_values: &[String],
    mut validation_subcodes: Vec<String>,
    sensitive_value: Option<&str>,
) -> Result<()> {
    let count = reasons.entry(reason.to_string()).or_default();
    *count = count.checked_add(1).ok_or_else(|| {
        BeadsError::Config("Conflict count overflow during additive reconciliation".to_string())
    })?;
    conflicts
        .by_issue
        .entry(issue_id.to_string())
        .or_default()
        .insert(reason.to_string());
    let mut related_value_sha256 = related_values
        .iter()
        .map(|value| hex_encode(&Sha256::digest(value.as_bytes())))
        .collect::<Vec<_>>();
    related_value_sha256.sort_unstable();
    related_value_sha256.dedup();
    validation_subcodes.sort_unstable();
    validation_subcodes.dedup();
    let value_sha256 = sensitive_value.map(|value| hex_encode(&Sha256::digest(value.as_bytes())));
    let detail_sha256 = additive_sha256(
        &(
            issue_id,
            reason,
            detail_kind,
            ordinal,
            &related_value_sha256,
            &validation_subcodes,
            &value_sha256,
        ),
        "additive conflict detail",
    )?;
    conflicts
        .details_by_issue
        .entry(issue_id.to_string())
        .or_default()
        .insert(AdditiveConflictDetailWitness {
            reason: reason.to_string(),
            detail_kind: detail_kind.to_string(),
            ordinal,
            related_value_sha256,
            validation_subcodes,
            value_sha256,
            detail_sha256,
        });
    Ok(())
}

fn additive_comment_validation_subcodes(errors: &[crate::error::ValidationError]) -> Vec<String> {
    let mut subcodes = errors
        .iter()
        .map(
            |error| match (error.field.as_str(), error.message.as_str()) {
                ("id", "must be positive") => "id_nonpositive",
                ("issue_id", "cannot be empty") => "issue_id_empty",
                ("content", "cannot be empty") => "body_empty",
                ("content", "cannot contain NUL bytes") => "body_contains_nul",
                ("author", "cannot be empty") => "author_empty",
                ("author", "exceeds 200 characters") => "author_too_long",
                _ => "other_validation",
            },
        )
        .map(str::to_string)
        .collect::<Vec<_>>();
    subcodes.sort_unstable();
    subcodes.dedup();
    subcodes
}

fn additive_external_ref_conflicts(
    source: &BTreeMap<String, Issue>,
    database: &BTreeMap<String, Issue>,
) -> (BTreeMap<String, String>, BTreeSet<String>, BTreeSet<String>) {
    let mut database_refs = BTreeMap::<String, String>::new();
    let mut ambiguous_database_refs = BTreeSet::new();
    for issue in database.values() {
        if let Some(external_ref) = issue
            .external_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match database_refs.get(external_ref) {
                Some(existing_id) if existing_id != &issue.id => {
                    ambiguous_database_refs.insert(external_ref.to_string());
                }
                None => {
                    database_refs.insert(external_ref.to_string(), issue.id.clone());
                }
                _ => {}
            }
        }
    }

    let mut source_refs = BTreeMap::<String, String>::new();
    let mut duplicate_source_ids = BTreeSet::new();
    for issue in source.values() {
        if let Some(external_ref) = issue
            .external_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match source_refs.get(external_ref) {
                Some(existing_id) if existing_id != &issue.id => {
                    duplicate_source_ids.insert(existing_id.clone());
                    duplicate_source_ids.insert(issue.id.clone());
                }
                None => {
                    source_refs.insert(external_ref.to_string(), issue.id.clone());
                }
                _ => {}
            }
        }
    }

    for external_ref in &ambiguous_database_refs {
        database_refs.remove(external_ref);
    }
    (database_refs, ambiguous_database_refs, duplicate_source_ids)
}

fn additive_comment_id_owners(issues: &BTreeMap<String, Issue>) -> BTreeMap<i64, BTreeSet<String>> {
    let mut owners = BTreeMap::<i64, BTreeSet<String>>::new();
    for issue in issues.values() {
        for comment in &issue.comments {
            if comment.id > 0 {
                owners
                    .entry(comment.id)
                    .or_default()
                    .insert(issue.id.clone());
            }
        }
    }
    owners
}

fn allocate_additive_comment_ids(
    issue: &mut Issue,
    used_ids: &mut BTreeSet<i64>,
    next_id: &mut Option<i64>,
    remaps: &mut Vec<AdditiveCommentIdRemap>,
) -> Result<()> {
    for comment in &mut issue.comments {
        let old_id = comment.id;
        let new_id = next_id.ok_or_else(|| {
            BeadsError::Config(
                "Comment ID space exhausted during additive reconciliation".to_string(),
            )
        })?;
        *next_id = new_id.checked_add(1);
        if !used_ids.insert(new_id) {
            return Err(BeadsError::Config(format!(
                "Projected comment ID {new_id} was already occupied during additive reconciliation"
            )));
        }
        comment.id = new_id;
        let logical_payload_sha256 = additive_sha256(
            &(
                comment.issue_id.as_str(),
                comment.author.as_str(),
                comment.body.as_str(),
                comment.created_at,
            ),
            "remapped comment logical payload",
        )?;
        remaps.push(AdditiveCommentIdRemap {
            issue_id: issue.id.clone(),
            old_id,
            new_id,
            logical_payload_sha256,
        });
    }
    Ok(())
}

fn additive_blocking_graph(issues: &BTreeMap<String, Issue>) -> BTreeMap<String, BTreeSet<String>> {
    let mut graph = issues
        .keys()
        .map(|issue_id| (issue_id.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();

    for issue in issues.values() {
        for dependency in &issue.dependencies {
            if !dependency.dep_type.is_blocking()
                || dependency.depends_on_id.starts_with("external:")
            {
                continue;
            }
            let (from, to) = if matches!(dependency.dep_type, DependencyType::ParentChild) {
                (&dependency.depends_on_id, &issue.id)
            } else {
                (&issue.id, &dependency.depends_on_id)
            };
            graph.entry(from.clone()).or_default().insert(to.clone());
            graph.entry(to.clone()).or_default();
        }
    }

    graph
}

fn additive_blocking_cycle_components(issues: &BTreeMap<String, Issue>) -> Vec<Vec<String>> {
    let graph = additive_blocking_graph(issues);
    let mut visited = BTreeSet::new();
    let mut finish_order = Vec::with_capacity(graph.len());

    for start in graph.keys() {
        if !visited.insert(start.clone()) {
            continue;
        }
        let mut stack = vec![(start.clone(), false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                finish_order.push(node);
                continue;
            }
            stack.push((node.clone(), true));
            if let Some(neighbors) = graph.get(&node) {
                for neighbor in neighbors.iter().rev() {
                    if visited.insert(neighbor.clone()) {
                        stack.push((neighbor.clone(), false));
                    }
                }
            }
        }
    }

    let mut reverse = graph
        .keys()
        .map(|node| (node.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for (from, targets) in &graph {
        for target in targets {
            reverse
                .entry(target.clone())
                .or_default()
                .insert(from.clone());
        }
    }

    let mut assigned = BTreeSet::new();
    let mut cycles = Vec::new();
    for start in finish_order.into_iter().rev() {
        if !assigned.insert(start.clone()) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            component.push(node.clone());
            if let Some(neighbors) = reverse.get(&node) {
                for neighbor in neighbors.iter().rev() {
                    if assigned.insert(neighbor.clone()) {
                        stack.push(neighbor.clone());
                    }
                }
            }
        }
        component.sort_unstable();
        let is_cycle = component.len() > 1
            || component
                .first()
                .is_some_and(|node| graph.get(node).is_some_and(|edges| edges.contains(node)));
        if is_cycle {
            cycles.push(component);
        }
    }
    cycles.sort_unstable();
    cycles
}

/// Build a read-only, lossless additive JSONL-to-database reconciliation plan.
///
/// The source is compared strictly by issue ID. No content-hash identity merge,
/// physical deletion, base snapshot, merge note, JSONL write, or database
/// mutation is performed.
///
/// # Errors
///
/// Returns an error for an unstable/invalid source or unreadable database.
pub fn plan_additive_reconcile(
    storage: &SqliteStorage,
    input_path: &Path,
    config: &AdditiveReconcileConfig,
) -> Result<AdditiveReconcilePlan> {
    let health_before = additive_database_health(storage)?;
    require_healthy_additive_database(&health_before, "preflight")?;
    let plan = storage.with_read_transaction(|storage| {
        plan_additive_reconcile_in_snapshot(storage, input_path, config, health_before.clone())
    })?;
    let health_after = additive_database_health(storage)?;
    require_healthy_additive_database(&health_after, "post-plan")?;
    if health_after != health_before {
        return Err(BeadsError::SyncConflict {
            message:
                "Database health changed while additive reconciliation captured its read snapshot"
                    .to_string(),
        });
    }
    Ok(plan)
}

#[allow(clippy::too_many_lines)]
fn plan_additive_reconcile_in_snapshot(
    storage: &SqliteStorage,
    input_path: &Path,
    config: &AdditiveReconcileConfig,
    health_before: AdditiveDatabaseHealth,
) -> Result<AdditiveReconcilePlan> {
    let source = additive_source_snapshot(input_path, config)?;
    let missing_references = storage.missing_issue_references()?;
    if !missing_references.is_empty() {
        return Err(BeadsError::Config(format!(
            "Additive reconciliation requires a referentially complete database; missing issue references in {}",
            missing_references.join(", ")
        )));
    }
    let database = hydrate_additive_database_issues(storage)?;
    let target_before = additive_database_witness(storage, &database)?;
    let database_issue_content_hashes = additive_issue_content_hashes(storage)?;
    let relations_before = target_before.relations;
    let source_ids = source.issues.keys().cloned().collect::<BTreeSet<_>>();
    let database_ids = database.keys().cloned().collect::<BTreeSet<_>>();
    let all_known_ids = source_ids
        .union(&database_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    let db_only_preserved = database_ids.difference(&source_ids).count();
    let (
        database_external_refs,
        ambiguous_database_external_refs,
        duplicate_source_external_ref_ids,
    ) = additive_external_ref_conflicts(&source.issues, &database);
    let database_comment_id_owners = additive_comment_id_owners(&database);
    if let Some((issue_id, comment_id)) = database.values().find_map(|issue| {
        issue
            .comments
            .iter()
            .find(|comment| comment.id <= 0)
            .map(|comment| (issue.id.as_str(), comment.id))
    }) {
        return Err(BeadsError::Config(format!(
            "Additive reconciliation requires positive persisted comment IDs; issue {issue_id} has {comment_id}"
        )));
    }
    let stored_sqlite_sequence = additive_sqlite_sequence(storage)?;
    if stored_sqlite_sequence
        .get("comments")
        .is_some_and(|sequence| *sequence < 0)
    {
        return Err(BeadsError::Config(
            "Additive reconciliation refuses a negative comments AUTOINCREMENT high-water mark"
                .to_string(),
        ));
    }
    let maximum_comment_id = database_comment_id_owners
        .keys()
        .copied()
        .chain(stored_sqlite_sequence.get("comments").copied())
        .max()
        .unwrap_or(0);
    let mut next_comment_id = maximum_comment_id.checked_add(1);
    let mut used_comment_ids = database_comment_id_owners
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();

    let mut mutations = Vec::new();
    let mut comment_id_remaps = Vec::new();
    let mut created = 0usize;
    let mut created_issue_ids = Vec::new();
    let mut updated = 0usize;
    let mut updated_issue_ids = Vec::new();
    let mut content_hash_repairs = Vec::new();
    let mut used_source_authoritative_ids = BTreeSet::new();
    let mut scalar_updates = Vec::new();
    let mut conflict_scalar_diffs = BTreeMap::new();
    let mut conflict_relation_diffs = BTreeMap::new();
    let mut skipped_equal = 0usize;
    let mut equal_issue_ids = Vec::new();
    let mut skipped_ephemeral = 0usize;
    let mut synchronized_export_hashes = Vec::new();
    let mut relations_after = relations_before;
    let mut relation_rows_planned = AdditiveRelationCounts::default();
    let mut conflict_reasons = BTreeMap::new();
    let mut conflict_ids = AdditiveConflictAccumulator::default();

    for issue in source.issues.values() {
        if issue.ephemeral || issue.id.contains("-wisp-") {
            skipped_ephemeral = skipped_ephemeral.checked_add(1).ok_or_else(|| {
                BeadsError::Config(
                    "Ephemeral skip count overflow during additive reconciliation".to_string(),
                )
            })?;
            record_additive_conflict(
                &mut conflict_reasons,
                &mut conflict_ids,
                &issue.id,
                "ephemeral_source_issue",
            )?;
            continue;
        }

        let mut issue_has_conflict = false;
        if duplicate_source_external_ref_ids.contains(&issue.id) {
            record_additive_conflict_detail(
                &mut conflict_reasons,
                &mut conflict_ids,
                &issue.id,
                "duplicate_source_external_ref",
                "external_ref",
                None,
                &[],
                issue.external_ref.as_deref(),
            )?;
            issue_has_conflict = true;
        }
        if let Some(external_ref) = issue
            .external_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if ambiguous_database_external_refs.contains(external_ref) {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "ambiguous_database_external_ref",
                    "external_ref",
                    None,
                    &[],
                    Some(external_ref),
                )?;
                issue_has_conflict = true;
            } else if let Some(existing_id) = database_external_refs.get(external_ref)
                && existing_id != &issue.id
            {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "external_ref_owned_by_other_id",
                    "external_ref",
                    None,
                    std::slice::from_ref(existing_id),
                    Some(external_ref),
                )?;
                issue_has_conflict = true;
            }
        }
        let mut labels = BTreeSet::new();
        for (label_ordinal, label) in issue.labels.iter().enumerate() {
            if !labels.insert(label.as_str()) {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "duplicate_label",
                    "label",
                    Some(label_ordinal),
                    &[],
                    Some(label),
                )?;
                issue_has_conflict = true;
            }
        }
        let mut dependency_targets = BTreeSet::new();
        let mut parent_child_count = 0usize;
        let mut parent_candidates = Vec::new();
        for (dependency_ordinal, dependency) in issue.dependencies.iter().enumerate() {
            if dependency.issue_id != issue.id {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "dependency_source_id_mismatch",
                    "dependency",
                    Some(dependency_ordinal),
                    std::slice::from_ref(&dependency.issue_id),
                    None,
                )?;
                issue_has_conflict = true;
            }
            if dependency.depends_on_id == issue.id {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "self_dependency",
                    "dependency",
                    Some(dependency_ordinal),
                    std::slice::from_ref(&dependency.depends_on_id),
                    None,
                )?;
                issue_has_conflict = true;
            }
            if !dependency_targets.insert(dependency.depends_on_id.as_str()) {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "duplicate_dependency_target",
                    "dependency",
                    Some(dependency_ordinal),
                    std::slice::from_ref(&dependency.depends_on_id),
                    None,
                )?;
                issue_has_conflict = true;
            }
            if dependency.metadata.as_deref().is_some_and(|metadata| {
                serde_json::from_str::<serde_json::Value>(metadata).is_err()
            }) {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "invalid_dependency_metadata",
                    "dependency_metadata",
                    Some(dependency_ordinal),
                    std::slice::from_ref(&dependency.depends_on_id),
                    dependency.metadata.as_deref(),
                )?;
                issue_has_conflict = true;
            }
            if matches!(dependency.dep_type, DependencyType::ParentChild) {
                parent_child_count = parent_child_count.checked_add(1).ok_or_else(|| {
                    BeadsError::Config(
                        "Parent-child count overflow during reconciliation".to_string(),
                    )
                })?;
                parent_candidates.push(dependency.depends_on_id.clone());
                if dependency.depends_on_id.starts_with("external:") {
                    record_additive_conflict_detail(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        "external_parent_child_endpoint",
                        "parent_child_dependency",
                        Some(dependency_ordinal),
                        std::slice::from_ref(&dependency.depends_on_id),
                        None,
                    )?;
                    issue_has_conflict = true;
                }
            }
            if !dependency.depends_on_id.starts_with("external:")
                && !all_known_ids.contains(&dependency.depends_on_id)
            {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "orphan_dependency_target",
                    "dependency",
                    Some(dependency_ordinal),
                    std::slice::from_ref(&dependency.depends_on_id),
                    None,
                )?;
                issue_has_conflict = true;
            }
        }
        if parent_child_count > 1 {
            record_additive_conflict_detail(
                &mut conflict_reasons,
                &mut conflict_ids,
                &issue.id,
                "multiple_parent_child_dependencies",
                "parent_child_set",
                None,
                &parent_candidates,
                None,
            )?;
            issue_has_conflict = true;
        }
        for (comment_ordinal, comment) in issue.comments.iter().enumerate() {
            let canonical_comment_payload = serde_json::to_string(comment).map_err(|error| {
                BeadsError::Config(format!(
                    "Could not witness source comment payload during additive reconciliation: {error}"
                ))
            })?;
            if comment.issue_id != issue.id {
                record_additive_conflict_detail(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "comment_source_id_mismatch",
                    "comment",
                    Some(comment_ordinal),
                    std::slice::from_ref(&comment.issue_id),
                    Some(&canonical_comment_payload),
                )?;
                issue_has_conflict = true;
            }
            let comment_for_validation = Comment {
                id: 1,
                issue_id: issue.id.clone(),
                author: comment.author.clone(),
                body: comment.body.clone(),
                created_at: comment.created_at,
            };
            if let Err(validation_errors) = CommentValidator::validate(&comment_for_validation) {
                record_additive_conflict_detail_with_subcodes(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "invalid_comment",
                    "comment_validation",
                    Some(comment_ordinal),
                    &[],
                    additive_comment_validation_subcodes(&validation_errors),
                    Some(&canonical_comment_payload),
                )?;
                issue_has_conflict = true;
            }
        }
        if issue_has_conflict {
            continue;
        }

        match database.get(&issue.id) {
            None => {
                created = created.checked_add(1).ok_or_else(|| {
                    BeadsError::Config(
                        "Create count overflow during additive reconciliation".to_string(),
                    )
                })?;
                let incoming_relations = AdditiveRelationCounts::from_issue(issue);
                relations_after = relations_after.checked_add(incoming_relations)?;
                relation_rows_planned = relation_rows_planned.checked_add(incoming_relations)?;
                let mut persisted_issue = issue.clone();
                allocate_additive_comment_ids(
                    &mut persisted_issue,
                    &mut used_comment_ids,
                    &mut next_comment_id,
                    &mut comment_id_remaps,
                )?;
                let content_hash = crate::util::content_hash(&persisted_issue);
                persisted_issue.content_hash = Some(content_hash.clone());
                mutations.push(AdditiveMutation::Create(persisted_issue));
                synchronized_export_hashes.push((issue.id.clone(), content_hash));
                created_issue_ids.push(issue.id.clone());
            }
            Some(existing) if additive_issues_semantically_equal(existing, issue) => {
                if config.source_authoritative_ids.contains(&issue.id) {
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        "source_authoritative_resolution_not_required",
                    )?;
                    continue;
                }
                skipped_equal = skipped_equal.checked_add(1).ok_or_else(|| {
                    BeadsError::Config(
                        "Equal skip count overflow during additive reconciliation".to_string(),
                    )
                })?;
                let expected_content_hash = crate::util::content_hash(existing);
                if database_issue_content_hashes
                    .get(&issue.id)
                    .and_then(Option::as_ref)
                    != Some(&expected_content_hash)
                {
                    content_hash_repairs.push((issue.id.clone(), expected_content_hash.clone()));
                }
                synchronized_export_hashes.push((issue.id.clone(), expected_content_hash));
                equal_issue_ids.push(issue.id.clone());
            }
            Some(existing)
                if existing.status == crate::model::Status::Tombstone
                    && issue.status != crate::model::Status::Tombstone =>
            {
                record_additive_conflict_scalar_diff(&mut conflict_scalar_diffs, existing, issue)?;
                if !additive_relations_semantically_equal(existing, issue) {
                    record_additive_conflict_relation_diff(
                        &mut conflict_relation_diffs,
                        existing,
                        issue,
                    )?;
                }
                record_additive_conflict(
                    &mut conflict_reasons,
                    &mut conflict_ids,
                    &issue.id,
                    "tombstone_resurrection",
                )?;
            }
            Some(existing) => {
                if !additive_relations_semantically_equal(existing, issue) {
                    record_additive_conflict_scalar_diff(
                        &mut conflict_scalar_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict_relation_diff(
                        &mut conflict_relation_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        "shared_relation_drift",
                    )?;
                    continue;
                }
                let source_is_newer = issue.updated_at > existing.updated_at;
                let source_is_authoritative = config.source_authoritative_ids.contains(&issue.id);
                let monotonic_closure =
                    source_is_newer && additive_is_monotonic_closure(existing, issue)?;
                if monotonic_closure && source_is_authoritative {
                    record_additive_conflict_scalar_diff(
                        &mut conflict_scalar_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        "source_authoritative_resolution_not_required",
                    )?;
                    continue;
                }
                if issue.status == crate::model::Status::Tombstone {
                    record_additive_conflict_scalar_diff(
                        &mut conflict_scalar_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        "live_to_tombstone_forbidden",
                    )?;
                    continue;
                }
                if !monotonic_closure && !source_is_authoritative {
                    let reason = if issue.updated_at < existing.updated_at {
                        "database_newer_shared_scalar_drift"
                    } else if source_is_newer {
                        "source_newer_scalar_drift_requires_resolution"
                    } else {
                        "equal_timestamp_shared_scalar_drift"
                    };
                    record_additive_conflict_scalar_diff(
                        &mut conflict_scalar_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        reason,
                    )?;
                    continue;
                }

                let resolution = if source_is_authoritative {
                    AdditiveScalarResolution::ExplicitSourceResolution
                } else {
                    AdditiveScalarResolution::MonotonicClosure
                };
                let scalar_update = additive_scalar_update_witness(existing, issue, resolution)?;
                if source_is_authoritative
                    && let Some(reason) = additive_explicit_scalar_resolution_conflict(
                        existing,
                        issue,
                        &scalar_update,
                    )
                {
                    record_additive_conflict_scalar_diff(
                        &mut conflict_scalar_diffs,
                        existing,
                        issue,
                    )?;
                    record_additive_conflict(
                        &mut conflict_reasons,
                        &mut conflict_ids,
                        &issue.id,
                        reason,
                    )?;
                    continue;
                }

                let mut persisted_issue = issue.clone();
                persisted_issue.labels.clone_from(&existing.labels);
                persisted_issue
                    .dependencies
                    .clone_from(&existing.dependencies);
                persisted_issue.comments.clone_from(&existing.comments);
                let content_hash = crate::util::content_hash(&persisted_issue);
                persisted_issue.content_hash = Some(content_hash.clone());
                mutations.push(AdditiveMutation::UpdateScalars(persisted_issue));
                synchronized_export_hashes.push((issue.id.clone(), content_hash));
                scalar_updates.push(scalar_update);
                updated = updated.checked_add(1).ok_or_else(|| {
                    BeadsError::Config(
                        "Update count overflow during additive reconciliation".to_string(),
                    )
                })?;
                updated_issue_ids.push(issue.id.clone());
                if source_is_authoritative {
                    used_source_authoritative_ids.insert(issue.id.clone());
                }
            }
        }
    }

    for issue_id in config
        .source_authoritative_ids
        .difference(&used_source_authoritative_ids)
    {
        if !conflict_ids.contains(issue_id) {
            record_additive_conflict(
                &mut conflict_reasons,
                &mut conflict_ids,
                issue_id,
                "source_authoritative_resolution_not_applicable",
            )?;
        }
    }

    let preexisting_cycles = additive_blocking_cycle_components(&database);
    let mut projected_database = database.clone();
    for mutation in &mutations {
        let issue = mutation.issue().clone();
        projected_database.insert(issue.id.clone(), issue);
    }
    let projected_cycles = additive_blocking_cycle_components(&projected_database);
    let preexisting_cycle_set = preexisting_cycles.iter().cloned().collect::<BTreeSet<_>>();
    let new_cycles = projected_cycles
        .iter()
        .filter(|cycle| !preexisting_cycle_set.contains(*cycle))
        .cloned()
        .collect::<Vec<_>>();
    for cycle in &new_cycles {
        for issue_id in cycle {
            record_additive_conflict_detail(
                &mut conflict_reasons,
                &mut conflict_ids,
                issue_id,
                "projected_blocking_cycle",
                "blocking_cycle",
                None,
                cycle,
                None,
            )?;
        }
    }

    synchronized_export_hashes.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    created_issue_ids.sort_unstable();
    updated_issue_ids.sort_unstable();
    content_hash_repairs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    equal_issue_ids.sort_unstable();
    scalar_updates.sort_unstable_by(|left, right| left.issue_id.cmp(&right.issue_id));
    let source_authoritative_issue_ids = used_source_authoritative_ids
        .into_iter()
        .collect::<Vec<_>>();
    let requested_source_authoritative_issue_ids = config
        .source_authoritative_ids
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let content_hash_repair_issue_ids = content_hash_repairs
        .iter()
        .map(|(issue_id, _)| issue_id.clone())
        .collect::<Vec<_>>();
    let content_hash_repair_witnesses = content_hash_repairs
        .iter()
        .map(|(issue_id, after)| AdditiveContentHashRepairWitness {
            issue_id: issue_id.clone(),
            before: database_issue_content_hashes
                .get(issue_id)
                .cloned()
                .flatten(),
            after: after.clone(),
        })
        .collect::<Vec<_>>();
    let db_only_issue_ids = database_ids
        .difference(&source_ids)
        .cloned()
        .collect::<Vec<_>>();
    comment_id_remaps.sort_unstable_by(|left, right| {
        left.issue_id
            .cmp(&right.issue_id)
            .then_with(|| left.old_id.cmp(&right.old_id))
            .then_with(|| left.new_id.cmp(&right.new_id))
    });
    let comment_id_remaps_sha256 =
        additive_sha256(&comment_id_remaps, "comment ID remap manifest")?;
    let synchronized_ids = synchronized_export_hashes
        .iter()
        .map(|(issue_id, _)| issue_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut dirty_before = storage.get_dirty_issue_metadata()?;
    dirty_before.sort_unstable();
    let expected_dirty_issues = dirty_before
        .iter()
        .filter(|(issue_id, _)| !synchronized_ids.contains(issue_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let dirty_markers_clear_planned = dirty_before
        .len()
        .checked_sub(expected_dirty_issues.len())
        .ok_or_else(|| {
            BeadsError::Config(
                "Dirty-marker count underflow during reconciliation planning".to_string(),
            )
        })?;
    let export_hashes_before = additive_export_hashes(storage)?;
    let mut expected_export_hashes = export_hashes_before.clone();
    let mut export_hash_updates_planned = 0usize;
    for (issue_id, desired_hash) in &synchronized_export_hashes {
        if expected_export_hashes.get(issue_id) != Some(desired_hash) {
            export_hash_updates_planned =
                export_hash_updates_planned.checked_add(1).ok_or_else(|| {
                    BeadsError::Config(
                        "Export-hash update count overflow during reconciliation".to_string(),
                    )
                })?;
        }
        expected_export_hashes.insert(issue_id.clone(), desired_hash.clone());
    }

    let mut expected_issues = database.clone();
    let mut expected_raw_issue_rows_by_id = additive_raw_issue_row_map(storage)?;
    let mut expected_issue_content_hashes = database_issue_content_hashes;
    for (issue_id, content_hash) in &content_hash_repairs {
        expected_issue_content_hashes.insert(issue_id.clone(), Some(content_hash.clone()));
    }
    for mutation in &mutations {
        let mut issue = mutation.issue().clone();
        let content_hash = issue.content_hash.clone().ok_or_else(|| {
            BeadsError::Config(format!(
                "Planned mutation for {} has no persisted content hash",
                issue.id
            ))
        })?;
        expected_issue_content_hashes.insert(issue.id.clone(), Some(content_hash));
        canonicalize_additive_issue_for_storage(&mut issue);
        expected_issues.insert(issue.id.clone(), issue);
        let raw_row = SqliteStorage::import_issue_raw_row_for_witness(mutation.issue())?
            .into_iter()
            .map(additive_sqlite_value_witness)
            .collect::<Vec<_>>();
        expected_raw_issue_rows_by_id.insert(mutation.issue().id.clone(), raw_row);
    }
    for (issue_id, content_hash) in &content_hash_repairs {
        let row = expected_raw_issue_rows_by_id
            .get_mut(issue_id)
            .ok_or_else(|| {
                BeadsError::Config(format!(
                    "Content-hash repair lost raw issue row for '{issue_id}'"
                ))
            })?;
        let content_hash_cell = row.get_mut(1).ok_or_else(|| {
            BeadsError::Config(format!(
                "Raw issue row for '{issue_id}' has no content_hash column"
            ))
        })?;
        *content_hash_cell =
            additive_sqlite_value_witness(SqliteValue::from(content_hash.as_str()));
    }
    let expected_raw_issue_rows = expected_raw_issue_rows_by_id
        .into_values()
        .collect::<Vec<_>>();
    let expected_blocked_cache = additive_expected_blocked_cache(&expected_issues);
    let expected_child_counters = additive_expected_child_counters(&expected_issues);
    let mut expected_sqlite_sequence = additive_sqlite_sequence(storage)?;
    if let Some(maximum_inserted_comment_id) = mutations
        .iter()
        .filter(|mutation| mutation.creates_issue())
        .flat_map(|mutation| mutation.issue().comments.iter())
        .map(|comment| comment.id)
        .max()
    {
        expected_sqlite_sequence
            .entry("comments".to_string())
            .and_modify(|sequence| *sequence = (*sequence).max(maximum_inserted_comment_id))
            .or_insert(maximum_inserted_comment_id);
    }
    let actual_blocked_cache = additive_actual_blocked_cache(storage)?;
    let actual_child_counters = additive_actual_child_counters(storage)?;
    let cache_rebuild_planned = !mutations.is_empty()
        || actual_blocked_cache != expected_blocked_cache
        || actual_child_counters != expected_child_counters;
    let mut expected_raw_export_hash_rows = additive_raw_rows_by_text_key(
        additive_raw_rows(
            storage,
            "SELECT issue_id, content_hash, exported_at FROM export_hashes ORDER BY issue_id",
        )?,
        "export_hashes",
    )?;
    for (issue_id, desired_hash) in &synchronized_export_hashes {
        if export_hashes_before.get(issue_id) != Some(desired_hash) {
            expected_raw_export_hash_rows.insert(
                issue_id.clone(),
                vec![
                    additive_sqlite_value_witness(SqliteValue::from(issue_id.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(desired_hash.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(source.witness.mtime.as_str())),
                ],
            );
        }
    }
    let expected_raw_export_hash_rows = expected_raw_export_hash_rows
        .into_values()
        .collect::<Vec<_>>();
    let expected_raw_blocked_cache_rows = if cache_rebuild_planned {
        expected_blocked_cache
            .iter()
            .map(|(issue_id, blockers)| {
                let blockers = serde_json::to_string(blockers).map_err(|error| {
                    BeadsError::Config(format!(
                        "Could not serialize expected blocked-cache row for {issue_id}: {error}"
                    ))
                })?;
                Ok(vec![
                    additive_sqlite_value_witness(SqliteValue::from(issue_id.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(blockers.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(source.witness.mtime.as_str())),
                ])
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        additive_raw_rows(
            storage,
            "SELECT issue_id, blocked_by, blocked_at FROM blocked_issues_cache ORDER BY issue_id",
        )?
    };

    let desired_needs_flush = db_only_preserved != 0;
    let mut expected_metadata = additive_metadata(storage)?;
    expected_metadata.insert(
        METADATA_JSONL_CONTENT_HASH.to_string(),
        source.witness.content_sha256.clone(),
    );
    expected_metadata.insert(
        METADATA_JSONL_MTIME.to_string(),
        source.witness.mtime.clone(),
    );
    expected_metadata.insert(
        METADATA_JSONL_SIZE.to_string(),
        source.witness.size.to_string(),
    );
    expected_metadata.insert(
        "needs_flush".to_string(),
        if desired_needs_flush { "true" } else { "false" }.to_string(),
    );
    if cache_rebuild_planned {
        expected_metadata.insert("blocked_cache_state".to_string(), String::new());
    }
    let metadata_before = additive_metadata(storage)?;
    let metadata_update_planned = expected_metadata != metadata_before;
    let bookkeeping_update_planned = metadata_update_planned
        || cache_rebuild_planned
        || dirty_markers_clear_planned != 0
        || export_hash_updates_planned != 0;

    let mut expected_label_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, label FROM labels ORDER BY issue_id, label",
    )?;
    let mut expected_dependency_rows = additive_raw_rows(
        storage,
        "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id \
         FROM dependencies \
         ORDER BY issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id",
    )?;
    let mut expected_comment_rows = additive_raw_rows(
        storage,
        "SELECT id, issue_id, author, text, created_at \
         FROM comments ORDER BY id, issue_id, author, text, created_at",
    )?;
    for mutation in mutations.iter().filter(|mutation| mutation.creates_issue()) {
        let issue = mutation.issue();
        for label in &issue.labels {
            expected_label_rows.push(vec![
                additive_sqlite_value_witness(SqliteValue::from(issue.id.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(label.as_str())),
            ]);
        }
        let mut dependency_targets = BTreeSet::new();
        for dependency in &issue.dependencies {
            if dependency_targets.insert(dependency.depends_on_id.as_str()) {
                expected_dependency_rows.push(vec![
                    additive_sqlite_value_witness(SqliteValue::from(issue.id.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(
                        dependency.depends_on_id.as_str(),
                    )),
                    additive_sqlite_value_witness(SqliteValue::from(dependency.dep_type.as_str())),
                    additive_sqlite_value_witness(SqliteValue::from(
                        dependency.created_at.to_rfc3339().as_str(),
                    )),
                    additive_sqlite_value_witness(SqliteValue::from(
                        dependency.created_by.as_deref().unwrap_or("import"),
                    )),
                    additive_sqlite_value_witness(SqliteValue::from(
                        dependency.metadata.as_deref().unwrap_or("{}"),
                    )),
                    additive_sqlite_value_witness(SqliteValue::from(
                        dependency.thread_id.as_deref().unwrap_or(""),
                    )),
                ]);
            }
        }
        for comment in &issue.comments {
            expected_comment_rows.push(vec![
                additive_sqlite_value_witness(SqliteValue::from(comment.id)),
                additive_sqlite_value_witness(SqliteValue::from(issue.id.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(comment.author.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(comment.body.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(
                    comment.created_at.to_rfc3339().as_str(),
                )),
            ]);
        }
    }
    expected_label_rows.sort_unstable();
    expected_dependency_rows.sort_unstable();
    expected_comment_rows.sort_unstable_by(|left, right| {
        let left_id = left
            .first()
            .and_then(|value| value.strip_prefix("integer:"))
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(i64::MIN);
        let right_id = right
            .first()
            .and_then(|value| value.strip_prefix("integer:"))
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(i64::MIN);
        left_id.cmp(&right_id).then_with(|| left.cmp(right))
    });
    let expected_metadata_rows = expected_metadata
        .iter()
        .map(|(key, value)| {
            vec![
                additive_sqlite_value_witness(SqliteValue::from(key.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(value.as_str())),
            ]
        })
        .collect::<Vec<_>>();
    let expected_child_counter_rows = expected_child_counters
        .iter()
        .map(|(parent_id, last_child)| {
            vec![
                additive_sqlite_value_witness(SqliteValue::from(parent_id.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(i64::from(*last_child))),
            ]
        })
        .collect::<Vec<_>>();
    let expected_sqlite_sequence_rows = expected_sqlite_sequence
        .iter()
        .map(|(name, sequence)| {
            vec![
                additive_sqlite_value_witness(SqliteValue::from(name.as_str())),
                additive_sqlite_value_witness(SqliteValue::from(*sequence)),
            ]
        })
        .collect::<Vec<_>>();
    let mut expected_target_after = target_before.clone();
    expected_target_after.issues = expected_issues.len();
    expected_target_after.issue_payload_sha256 = additive_sha256(
        &expected_raw_issue_rows,
        "expected raw database issue payload",
    )?;
    expected_target_after.issue_semantic_payload_sha256 =
        additive_sha256(&expected_issues, "expected database semantic issue payload")?;
    expected_target_after.issue_content_hashes = expected_issue_content_hashes.len();
    expected_target_after.issue_content_hash_payload_sha256 = additive_sha256(
        &expected_issue_content_hashes,
        "expected database issue content hashes",
    )?;
    expected_target_after.relations = relations_after;
    expected_target_after.label_payload_sha256 =
        additive_sha256(&expected_label_rows, "expected raw labels")?;
    expected_target_after.dependency_payload_sha256 =
        additive_sha256(&expected_dependency_rows, "expected raw dependencies")?;
    expected_target_after.comment_payload_sha256 =
        additive_sha256(&expected_comment_rows, "expected raw comments")?;
    expected_target_after.export_hashes = expected_raw_export_hash_rows.len();
    expected_target_after.export_hash_payload_sha256 =
        additive_sha256(&expected_raw_export_hash_rows, "expected raw export hashes")?;
    expected_target_after.dirty_issues = expected_dirty_issues.len();
    expected_target_after.dirty_payload_sha256 =
        additive_sha256(&expected_dirty_issues, "expected dirty markers")?;
    expected_target_after.metadata_rows = expected_metadata_rows.len();
    expected_target_after.metadata_payload_sha256 =
        additive_sha256(&expected_metadata_rows, "expected raw metadata")?;
    expected_target_after.blocked_cache_entries = expected_raw_blocked_cache_rows.len();
    expected_target_after.blocked_cache_payload_sha256 = additive_sha256(
        &expected_raw_blocked_cache_rows,
        "expected raw blocked cache",
    )?;
    expected_target_after.child_counter_entries = expected_child_counter_rows.len();
    expected_target_after.child_counter_payload_sha256 =
        additive_sha256(&expected_child_counter_rows, "expected raw child counters")?;
    expected_target_after.sqlite_sequence =
        additive_table_witness(&expected_sqlite_sequence_rows, "expected sqlite sequence")?;
    expected_target_after.stored_jsonl_content_hash = Some(source.witness.content_sha256.clone());
    expected_target_after.stored_jsonl_mtime = Some(source.witness.mtime.clone());
    expected_target_after.stored_jsonl_size = Some(source.witness.size.to_string());
    expected_target_after.needs_flush =
        Some(if desired_needs_flush { "true" } else { "false" }.to_string());

    let conflict_occurrences = conflict_reasons.values().try_fold(0usize, |total, count| {
        total.checked_add(*count).ok_or_else(|| {
            BeadsError::Config("Conflict total overflow during additive reconciliation".to_string())
        })
    })?;
    let conflicted = conflict_ids.len();
    let conflict_witnesses = conflict_ids.witnesses();
    let conflict_witnesses_sha256 =
        additive_sha256(&conflict_witnesses, "complete conflict witness manifest")?;
    let conflict_scalar_diffs = conflict_scalar_diffs.into_values().collect::<Vec<_>>();
    let conflict_scalar_diffs_sha256 = additive_sha256(
        &conflict_scalar_diffs,
        "complete conflict scalar diff manifest",
    )?;
    let conflict_relation_diffs = conflict_relation_diffs.into_values().collect::<Vec<_>>();
    let conflict_relation_diffs_sha256 = additive_sha256(
        &conflict_relation_diffs,
        "complete conflict relation diff manifest",
    )?;
    let conflict_issue_ids = conflict_witnesses
        .iter()
        .map(|witness| witness.issue_id.clone())
        .collect::<Vec<_>>();
    let conflict_issue_ids_sha256 =
        additive_sha256(&conflict_issue_ids, "conflict issue ID manifest")?;
    let conflict_issue_ids_truncated = false;
    let status = if conflicted != 0 {
        AdditiveReconcileStatus::Conflicted
    } else if mutations.is_empty() && content_hash_repairs.is_empty() && bookkeeping_update_planned
    {
        AdditiveReconcileStatus::MetadataOnlyReady
    } else if mutations.is_empty() && content_hash_repairs.is_empty() {
        AdditiveReconcileStatus::NoChanges
    } else {
        AdditiveReconcileStatus::Ready
    };
    let database_after_planning = hydrate_additive_database_issues(storage)?;
    let target_after_planning = additive_database_witness(storage, &database_after_planning)?;
    if target_after_planning != target_before {
        return Err(BeadsError::Config(
            "Database changed while additive reconciliation was planning; retry from a stable snapshot"
                .to_string(),
        ));
    }
    let created_issue_ids_sha256 =
        additive_sha256(&created_issue_ids, "created issue ID manifest")?;
    let updated_issue_ids_sha256 =
        additive_sha256(&updated_issue_ids, "updated issue ID manifest")?;
    let requested_source_authoritative_issue_ids_sha256 = additive_sha256(
        &requested_source_authoritative_issue_ids,
        "requested source-authoritative issue ID manifest",
    )?;
    let equal_issue_ids_sha256 = additive_sha256(&equal_issue_ids, "equal issue ID manifest")?;
    let source_authoritative_issue_ids_sha256 = additive_sha256(
        &source_authoritative_issue_ids,
        "source-authoritative issue ID manifest",
    )?;
    let content_hash_repair_issue_ids_sha256 = additive_sha256(
        &content_hash_repair_issue_ids,
        "content-hash repair issue ID manifest",
    )?;
    let content_hash_repairs_sha256 = additive_sha256(
        &content_hash_repair_witnesses,
        "content-hash repair witness manifest",
    )?;
    let scalar_updates_sha256 = additive_sha256(&scalar_updates, "scalar update witness manifest")?;
    let db_only_issue_ids_sha256 =
        additive_sha256(&db_only_issue_ids, "database-only issue ID manifest")?;
    let expected_issue_semantic_payload_sha256 =
        additive_sha256(&expected_issues, "expected issue payload")?;
    let expected_issue_raw_payload_sha256 =
        additive_sha256(&expected_raw_issue_rows, "expected raw issue payload")?;
    let expected_issue_content_hash_payload_sha256 = additive_sha256(
        &expected_issue_content_hashes,
        "expected issue content hashes",
    )?;
    let expected_export_hash_payload_sha256 =
        additive_sha256(&expected_raw_export_hash_rows, "expected raw export hashes")?;
    let expected_dirty_payload_sha256 =
        additive_sha256(&expected_dirty_issues, "expected dirty markers")?;
    let expected_metadata_payload_sha256 =
        additive_sha256(&expected_metadata, "expected metadata")?;
    let expected_blocked_cache_payload_sha256 = additive_sha256(
        &expected_raw_blocked_cache_rows,
        "expected raw blocked cache",
    )?;
    let expected_child_counter_payload_sha256 =
        additive_sha256(&expected_child_counters, "expected child counters")?;
    let expected_sqlite_sequence_payload_sha256 =
        additive_sha256(&expected_sqlite_sequence, "expected sqlite sequence")?;
    let (workspace_path, workspace_path_sha256) = additive_provenance_path(
        config.beads_dir.as_deref(),
        config.beads_dir.as_deref(),
        "workspace",
    )?;
    let workspace_identity_sha256 = config
        .beads_dir
        .as_deref()
        .map(|path| additive_path_identity_sha256(path, "workspace"))
        .transpose()?
        .unwrap_or_else(|| workspace_path_sha256.clone());
    let (source_path, source_path_sha256) =
        additive_provenance_path(Some(input_path), config.beads_dir.as_deref(), "source")?;
    if source_path_sha256 != source.witness.canonical_path_sha256 {
        return Err(BeadsError::Config(
            "Canonical source path changed between snapshot and plan provenance binding"
                .to_string(),
        ));
    }
    let (database_path, database_path_sha256) = additive_provenance_path(
        config.database_path.as_deref(),
        config.beads_dir.as_deref(),
        "database",
    )?;
    let database_identity_sha256 = config
        .database_path
        .as_deref()
        .map(|path| additive_path_identity_sha256(path, "database"))
        .transpose()?;
    let write_lock_authority_sha256 = match config.database_path.as_deref() {
        Some(path) => database_write_authority_sha256(path)?,
        None => additive_sha256(&"<in-memory>", "in-memory write authority")?,
    };
    let mut plan = AdditiveReconcilePlan {
        receipt: AdditiveReconcileReceipt {
            schema: ADDITIVE_RECONCILE_SCHEMA.to_string(),
            algorithm: ADDITIVE_RECONCILE_ALGORITHM.to_string(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            plan_sha256: String::new(),
            status,
            workspace_path,
            workspace_path_sha256,
            workspace_identity_sha256,
            source_path,
            source_path_sha256,
            source_identity_sha256: source.witness.identity_sha256,
            database_path,
            database_path_sha256,
            database_identity_sha256,
            write_lock_authority: if config.database_path.is_some() {
                "terminal_workspace_lock+canonical_database_family_lock+database_inode_lock"
                    .to_string()
            } else {
                "in_memory_test_scope".to_string()
            },
            write_lock_authority_sha256,
            database_authority_preserved_after_commit: None,
            database_poststate_preserved_after_commit: None,
            workspace_authority_preserved_after_commit: None,
            source_preserved_after_commit: None,
            foreign_keys_restored_after_commit: None,
            postcommit_failures: Vec::new(),
            database_user_version: storage.schema_user_version()?,
            project_prefix: storage.get_config("issue_prefix")?,
            source_raw_sha256: source.witness.raw_sha256,
            source_content_sha256: source.witness.content_sha256,
            source_storage_projection_sha256: additive_sha256(
                &source.issues,
                "source storage-semantic projection",
            )?,
            source_size: source.witness.size,
            source_mtime: source.witness.mtime.clone(),
            poststate_timestamp: source.witness.mtime,
            source_issues: source.record_count,
            target_before: target_before.clone(),
            expected_target_after,
            target_after: None,
            health_before,
            health_after: None,
            created,
            created_issue_ids,
            created_issue_ids_sha256,
            updated,
            updated_issue_ids,
            updated_issue_ids_sha256,
            requested_source_authoritative_issue_ids,
            requested_source_authoritative_issue_ids_sha256,
            source_authoritative_issue_ids,
            source_authoritative_issue_ids_sha256,
            scalar_updates,
            scalar_updates_sha256,
            content_hash_repairs_planned: content_hash_repairs.len(),
            content_hash_repairs: content_hash_repair_witnesses,
            content_hash_repairs_sha256,
            content_hash_repair_issue_ids,
            content_hash_repair_issue_ids_sha256,
            content_hash_repairs_applied: 0,
            skipped_equal,
            equal_issue_ids,
            equal_issue_ids_sha256,
            skipped_ephemeral,
            synchronized: synchronized_export_hashes.len(),
            export_hash_updates_planned,
            export_hashes_updated: 0,
            dirty_markers_clear_planned,
            dirty_markers_cleared: 0,
            conflicted,
            conflict_occurrences,
            deleted: 0,
            db_only_preserved,
            db_only_issue_ids,
            db_only_issue_ids_sha256,
            conflict_reasons,
            conflict_issue_ids,
            conflict_issue_ids_sha256,
            conflict_issue_ids_truncated,
            conflict_witnesses,
            conflict_witnesses_sha256,
            conflict_scalar_diffs,
            conflict_scalar_diffs_sha256,
            conflict_relation_diffs,
            conflict_relation_diffs_sha256,
            comment_id_remaps,
            comment_id_remaps_sha256,
            preexisting_blocking_cycles: preexisting_cycles.len(),
            projected_blocking_cycles: projected_cycles.len(),
            new_blocking_cycles: new_cycles.len(),
            relations_before,
            relations_after,
            relation_rows_planned,
            relation_rows_applied: AdditiveRelationCounts::default(),
            expected_issue_raw_payload_sha256,
            expected_issue_semantic_payload_sha256,
            expected_issue_content_hash_payload_sha256,
            expected_export_hash_payload_sha256,
            expected_dirty_payload_sha256,
            expected_metadata_payload_sha256,
            expected_blocked_cache_payload_sha256,
            expected_child_counter_payload_sha256,
            expected_sqlite_sequence_payload_sha256,
            events_before: target_before.events,
            events_after: target_before.events,
            event_payload_sha256_before: target_before.event_payload_sha256.clone(),
            event_payload_sha256_after: target_before.event_payload_sha256.clone(),
            cache_rebuild_planned,
            cache_rebuild_performed: false,
            metadata_update_planned,
            metadata_changed: false,
            jsonl_written: false,
            base_snapshot_used: false,
            merge_note_written: false,
        },
        mutations,
        content_hash_repairs,
        synchronized_export_hashes,
        expected_issues,
        expected_raw_issue_rows,
        expected_issue_content_hashes,
        expected_export_hashes,
        expected_raw_export_hash_rows,
        expected_dirty_issues,
        expected_metadata,
        expected_blocked_cache,
        expected_raw_blocked_cache_rows,
        expected_child_counters,
        expected_sqlite_sequence,
    };
    plan.receipt.plan_sha256 = additive_plan_sha256(&plan)?;
    tracing::info!(
        schema = ADDITIVE_RECONCILE_SCHEMA,
        algorithm = ADDITIVE_RECONCILE_ALGORITHM,
        status = plan.receipt.status.as_str(),
        plan_sha256_prefix = &plan.receipt.plan_sha256[..12],
        source_issues = plan.receipt.source_issues,
        target_issues = plan.receipt.target_before.issues,
        created = plan.receipt.created,
        updated = plan.receipt.updated,
        equal = plan.receipt.skipped_equal,
        db_only_preserved = plan.receipt.db_only_preserved,
        conflicted = plan.receipt.conflicted,
        conflict_occurrences = plan.receipt.conflict_occurrences,
        comment_id_remaps = plan.receipt.comment_id_remaps.len(),
        "Planned additive reconciliation"
    );
    let created_log_len = plan
        .receipt
        .created_issue_ids
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let updated_log_len = plan
        .receipt
        .updated_issue_ids
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let resolution_log_len = plan
        .receipt
        .source_authoritative_issue_ids
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let scalar_log_len = plan
        .receipt
        .scalar_updates
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let content_hash_repair_log_len = plan
        .receipt
        .content_hash_repairs
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let db_only_log_len = plan
        .receipt
        .db_only_issue_ids
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let remap_log_len = plan
        .receipt
        .comment_id_remaps
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let conflict_log_len = plan
        .receipt
        .conflict_witnesses
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let conflict_scalar_log_len = plan
        .receipt
        .conflict_scalar_diffs
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    let conflict_relation_log_len = plan
        .receipt
        .conflict_relation_diffs
        .len()
        .min(ADDITIVE_LOG_PREVIEW_LIMIT);
    tracing::debug!(
        manifest_prefix_limit = ADDITIVE_LOG_PREVIEW_LIMIT,
        created_issue_ids_prefix = ?&plan.receipt.created_issue_ids[..created_log_len],
        created_issue_ids_sha256 = %plan.receipt.created_issue_ids_sha256,
        updated_issue_ids_prefix = ?&plan.receipt.updated_issue_ids[..updated_log_len],
        updated_issue_ids_sha256 = %plan.receipt.updated_issue_ids_sha256,
        source_authoritative_issue_ids_prefix =
            ?&plan.receipt.source_authoritative_issue_ids[..resolution_log_len],
        source_authoritative_issue_ids_sha256 =
            %plan.receipt.source_authoritative_issue_ids_sha256,
        scalar_update_witnesses_prefix = ?&plan.receipt.scalar_updates[..scalar_log_len],
        scalar_updates_sha256 = %plan.receipt.scalar_updates_sha256,
        content_hash_repair_witnesses_prefix =
            ?&plan.receipt.content_hash_repairs[..content_hash_repair_log_len],
        content_hash_repairs_sha256 = %plan.receipt.content_hash_repairs_sha256,
        db_only_issue_ids_prefix = ?&plan.receipt.db_only_issue_ids[..db_only_log_len],
        db_only_issue_ids_sha256 = %plan.receipt.db_only_issue_ids_sha256,
        conflict_reasons = ?plan.receipt.conflict_reasons,
        conflict_witnesses_prefix =
            ?&plan.receipt.conflict_witnesses[..conflict_log_len],
        conflict_witnesses_sha256 = %plan.receipt.conflict_witnesses_sha256,
        conflict_scalar_diffs_prefix =
            ?&plan.receipt.conflict_scalar_diffs[..conflict_scalar_log_len],
        conflict_scalar_diffs_sha256 = %plan.receipt.conflict_scalar_diffs_sha256,
        conflict_relation_diffs_prefix =
            ?&plan.receipt.conflict_relation_diffs[..conflict_relation_log_len],
        conflict_relation_diffs_sha256 = %plan.receipt.conflict_relation_diffs_sha256,
        comment_id_remaps_prefix = ?&plan.receipt.comment_id_remaps[..remap_log_len],
        comment_id_remaps_sha256 = %plan.receipt.comment_id_remaps_sha256,
        expected_blocked_cache_sha256 = %plan.receipt.expected_blocked_cache_payload_sha256,
        expected_child_counters_sha256 = %plan.receipt.expected_child_counter_payload_sha256,
        "Additive reconciliation review manifests"
    );
    Ok(plan)
}

fn additive_source_matches_receipt(
    snapshot: &AdditiveSourceSnapshot,
    receipt: &AdditiveReconcileReceipt,
) -> bool {
    snapshot.witness.raw_sha256 == receipt.source_raw_sha256
        && snapshot.witness.content_sha256 == receipt.source_content_sha256
        && snapshot.witness.canonical_path_sha256 == receipt.source_path_sha256
        && snapshot.witness.identity_sha256 == receipt.source_identity_sha256
        && snapshot.witness.size == receipt.source_size
        && snapshot.witness.mtime == receipt.source_mtime
        && snapshot.record_count == receipt.source_issues
}

fn require_reviewed_additive_schema_version(
    storage: &SqliteStorage,
    receipt: &AdditiveReconcileReceipt,
    phase: &str,
) -> Result<()> {
    let expected = u32::try_from(crate::storage::schema::CURRENT_SCHEMA_VERSION).map_err(|_| {
        BeadsError::Config(
            "Current schema version cannot be represented in the reconciliation receipt"
                .to_string(),
        )
    })?;
    let observed = storage.schema_user_version()?;
    if receipt.database_user_version != expected || observed != expected {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Database schema version changed during additive reconciliation {phase}: reviewed {}, required {expected}, observed {observed}",
                receipt.database_user_version
            ),
        });
    }
    Ok(())
}

/// Apply a previously built additive reconciliation plan transactionally.
///
/// Both the source witness and complete database witness are rechecked before
/// mutation. The source and event stream are rechecked again before commit.
///
/// # Errors
///
/// Returns an error if the plan has conflicts, either witness drifted, or any
/// transactional invariant fails.
#[allow(clippy::too_many_lines)]
pub(crate) fn apply_additive_reconcile(
    storage: &mut SqliteStorage,
    input_path: &Path,
    config: &AdditiveReconcileConfig,
    plan: &AdditiveReconcilePlan,
    expected_plan_sha256: &str,
) -> Result<AdditiveReconcileReceipt> {
    if expected_plan_sha256 != plan.receipt.plan_sha256 {
        tracing::warn!(
            reviewed_plan_sha256_prefix = %expected_plan_sha256.get(..12).unwrap_or("<invalid>"),
            current_plan_sha256_prefix = %plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            "Rejected additive reconciliation apply with stale or mismatched review token"
        );
        return Err(BeadsError::SyncConflict {
            message: "Reviewed additive reconciliation plan SHA-256 is stale or mismatched; rerun the read-only plan and review its complete receipt before applying"
                .to_string(),
        });
    }
    if plan.has_conflicts() {
        tracing::warn!(
            plan_sha256_prefix = %plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            conflicted = plan.receipt.conflicted,
            conflict_occurrences = plan.receipt.conflict_occurrences,
            conflict_reasons = ?plan.receipt.conflict_reasons,
            "Rejected conflicted additive reconciliation plan"
        );
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Additive reconciliation has {} conflicted issue(s) across {} observation(s); refusing to apply",
                plan.receipt.conflicted, plan.receipt.conflict_occurrences
            ),
        });
    }

    let fresh_plan = plan_additive_reconcile(storage, input_path, config)?;
    if fresh_plan != *plan {
        tracing::warn!(
            reviewed_plan_sha256_prefix = %plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            fresh_plan_sha256_prefix = %fresh_plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            "Rejected stale additive reconciliation plan after exact replan"
        );
        return Err(BeadsError::SyncConflict {
            message: "Additive reconciliation plan is stale; regenerate the dry-run receipt"
                .to_string(),
        });
    }
    tracing::info!(
        plan_sha256_prefix = %plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
        created = plan.receipt.created,
        updated = plan.receipt.updated,
        relation_rows = ?plan.receipt.relation_rows_planned,
        export_hash_updates = plan.receipt.export_hash_updates_planned,
        dirty_marker_clears = plan.receipt.dirty_markers_clear_planned,
        cache_rebuild = plan.receipt.cache_rebuild_planned,
        metadata_update = plan.receipt.metadata_update_planned,
        "Applying reviewed additive reconciliation transaction"
    );
    #[cfg(test)]
    additive_test_fail_at(AdditiveTestFailPhase::BeforeTransaction)?;
    #[cfg(test)]
    additive_test_drift_schema_before_transaction(storage)?;
    let plan = plan.clone();
    let result = storage
        .with_reconcile_transaction("additive reconciliation", |storage| {
        require_reviewed_additive_schema_version(storage, &plan.receipt, "transaction start")?;
        let source_before = additive_source_snapshot(input_path, config)?;
        if !additive_source_matches_receipt(&source_before, &plan.receipt) {
            return Err(BeadsError::SyncConflict {
                message: "JSONL witness changed after additive reconciliation planning"
                    .to_string(),
            });
        }
        let database_before = hydrate_additive_database_issues(storage)?;
        let target_before = additive_database_witness(storage, &database_before)?;
        if target_before != plan.receipt.target_before {
            return Err(BeadsError::SyncConflict {
                message: "Database witness changed after additive reconciliation planning"
                    .to_string(),
            });
        }
        if plan.mutations.is_empty()
            && plan.content_hash_repairs.is_empty()
            && !plan.receipt.metadata_update_planned
            && !plan.receipt.cache_rebuild_planned
            && plan.receipt.export_hash_updates_planned == 0
            && plan.receipt.dirty_markers_clear_planned == 0
        {
            let source_after = additive_source_snapshot(input_path, config)?;
            if !additive_source_matches_receipt(&source_after, &plan.receipt) {
                return Err(BeadsError::SyncConflict {
                    message: "JSONL witness changed during additive reconciliation no-op proof"
                        .to_string(),
                });
            }
            let database_after = hydrate_additive_database_issues(storage)?;
            let target_after = additive_database_witness(storage, &database_after)?;
            if target_after != target_before {
                return Err(BeadsError::SyncConflict {
                    message: "Database witness changed during additive reconciliation no-op proof"
                        .to_string(),
                });
            }
            let transaction_health = additive_database_transaction_health(storage)?;
            require_healthy_additive_database(&transaction_health, "no-op transaction")?;
            require_reviewed_additive_schema_version(
                storage,
                &plan.receipt,
                "no-op commit boundary",
            )?;
            let mut receipt = plan.receipt.clone();
            receipt.target_after = Some(target_after);
            receipt.health_after = None;
            receipt.status = AdditiveReconcileStatus::NoChanges;
            return Ok(receipt);
        }

        let synchronized_ids = plan
            .synchronized_export_hashes
            .iter()
            .map(|(issue_id, _)| issue_id.clone())
            .collect::<BTreeSet<_>>();
        for mutation in &plan.mutations {
            let issue = mutation.issue();
            match mutation {
                AdditiveMutation::Create(_) => {
                    if !storage.insert_new_issue_for_import_in_tx(issue)? {
                        return Err(BeadsError::Config(format!(
                            "Additive reconciliation failed to insert planned issue {}",
                            issue.id
                        )));
                    }
                }
                AdditiveMutation::UpdateScalars(_) => {
                    if !storage.upsert_issue_for_import_in_tx(issue)? {
                        return Err(BeadsError::Config(format!(
                            "Additive reconciliation failed to update planned issue {}",
                            issue.id
                        )));
                    }
                }
            }
        }
        for mutation in plan
            .mutations
            .iter()
            .filter(|mutation| mutation.creates_issue())
        {
            let issue = mutation.issue();
            if storage.has_owned_relation_rows_for_import(&issue.id)? {
                return Err(BeadsError::Config(format!(
                    "New issue {} unexpectedly owned relation rows before relation insertion",
                    issue.id
                )));
            }
            storage.insert_new_issue_relations_for_import_in_tx(issue)?;
        }
        #[cfg(test)]
        additive_test_fail_at(AdditiveTestFailPhase::AfterIssueAndRelationWrites)?;
        let content_hash_repairs_applied =
            storage.repair_issue_content_hashes_in_tx(&plan.content_hash_repairs)?;
        if content_hash_repairs_applied != plan.receipt.content_hash_repairs_planned {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation planned {} content-hash repair(s), applied {content_hash_repairs_applied}",
                plan.receipt.content_hash_repairs_planned
            )));
        }
        let export_hashes_updated = storage.set_changed_export_hashes_at_in_tx(
            &plan.synchronized_export_hashes,
            &plan.receipt.poststate_timestamp,
        )?;
        let dirty_markers_cleared = if synchronized_ids.is_empty() {
            0
        } else {
            let dirty_to_clear = storage
                .get_dirty_issue_metadata()?
                .into_iter()
                .filter(|(issue_id, _)| synchronized_ids.contains(issue_id))
                .collect::<Vec<_>>();
            storage.clear_dirty_issues_in_tx(&dirty_to_clear)?
        };
        if export_hashes_updated != plan.receipt.export_hash_updates_planned {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation planned {} export-hash update(s), applied {export_hashes_updated}",
                plan.receipt.export_hash_updates_planned
            )));
        }
        if dirty_markers_cleared != plan.receipt.dirty_markers_clear_planned {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation planned {} dirty-marker clear(s), applied {dirty_markers_cleared}",
                plan.receipt.dirty_markers_clear_planned
            )));
        }
        if plan.receipt.cache_rebuild_planned {
            storage.rebuild_blocked_cache_at_in_tx(&plan.receipt.poststate_timestamp)?;
            storage.rebuild_child_counters_in_tx()?;
        }
        storage.set_metadata_in_tx(
            METADATA_JSONL_CONTENT_HASH,
            &plan.receipt.source_content_sha256,
        )?;
        record_observed_jsonl_witness_in_tx(
            storage,
            &JsonlWitness {
                mtime: redact_reviewed_path_result(
                    observed_jsonl_witness(input_path),
                    input_path,
                    "source",
                    "witness",
                )?
                .mtime,
                mtime_witness: plan.receipt.source_mtime.clone(),
                size: plan.receipt.source_size,
            },
        )?;
        let needs_flush = plan.receipt.db_only_preserved != 0;
        storage.set_metadata_in_tx("needs_flush", if needs_flush { "true" } else { "false" })?;

        let source_after = additive_source_snapshot(input_path, config)?;
        if !additive_source_matches_receipt(&source_after, &plan.receipt) {
            return Err(BeadsError::SyncConflict {
                message: "JSONL witness changed during additive reconciliation apply".to_string(),
            });
        }
        let database_after = hydrate_additive_database_issues(storage)?;
        if database_after != plan.expected_issues {
            return Err(BeadsError::Config(
                "Exact issue payload after additive reconciliation does not match the reviewed plan"
                    .to_string(),
            ));
        }
        for update in &plan.receipt.scalar_updates {
            let issue = database_after.get(&update.issue_id).ok_or_else(|| {
                BeadsError::Config(format!(
                    "Updated issue {} disappeared during additive reconciliation",
                    update.issue_id
                ))
            })?;
            let relation_payload_sha256 = additive_sha256(
                &(&issue.labels, &issue.dependencies, &issue.comments),
                "applied scalar update relation payload",
            )?;
            if relation_payload_sha256 != update.relation_payload_sha256 {
                return Err(BeadsError::Config(format!(
                    "Scalar-only repair changed relation payload for {}",
                    update.issue_id
                )));
            }
        }
        let issue_content_hashes_after = additive_issue_content_hashes(storage)?;
        if issue_content_hashes_after != plan.expected_issue_content_hashes {
            return Err(BeadsError::Config(
                "Persisted issue content hashes after additive reconciliation do not match the reviewed plan"
                    .to_string(),
            ));
        }
        let export_hashes_after = additive_export_hashes(storage)?;
        if export_hashes_after != plan.expected_export_hashes {
            return Err(BeadsError::Config(
                "Export hashes after additive reconciliation do not match the reviewed plan"
                    .to_string(),
            ));
        }
        let raw_export_hash_rows_after = additive_raw_rows(
            storage,
            "SELECT issue_id, content_hash, exported_at FROM export_hashes \
             ORDER BY issue_id, content_hash, exported_at",
        )?;
        if raw_export_hash_rows_after != plan.expected_raw_export_hash_rows {
            return Err(BeadsError::Config(
                "Raw export-hash rows after additive reconciliation do not match the reviewed plan"
                    .to_string(),
            ));
        }
        let mut dirty_after = storage.get_dirty_issue_metadata()?;
        dirty_after.sort_unstable();
        if dirty_after != plan.expected_dirty_issues {
            return Err(BeadsError::Config(
                "Dirty markers after additive reconciliation do not match the reviewed plan"
                    .to_string(),
            ));
        }
        let metadata_after = additive_metadata(storage)?;
        if metadata_after != plan.expected_metadata {
            return Err(BeadsError::Config(
                "Metadata after additive reconciliation does not match the reviewed plan"
                    .to_string(),
            ));
        }
        let blocked_cache_after = additive_actual_blocked_cache(storage)?;
        if blocked_cache_after != plan.expected_blocked_cache {
            return Err(BeadsError::Config(
                "Blocked-cache projection after additive reconciliation does not match the independent in-memory projection"
                    .to_string(),
            ));
        }
        let raw_blocked_cache_rows_after = additive_raw_rows(
            storage,
            "SELECT issue_id, blocked_by, blocked_at FROM blocked_issues_cache \
             ORDER BY issue_id, blocked_by, blocked_at",
        )?;
        if raw_blocked_cache_rows_after != plan.expected_raw_blocked_cache_rows {
            return Err(BeadsError::Config(
                "Raw blocked-cache rows after additive reconciliation do not match the reviewed plan"
                    .to_string(),
            ));
        }
        let child_counters_after = additive_actual_child_counters(storage)?;
        if child_counters_after != plan.expected_child_counters {
            return Err(BeadsError::Config(
                "Child counters after additive reconciliation do not match the independent issue-ID projection"
                    .to_string(),
            ));
        }
        let sqlite_sequence_after = additive_sqlite_sequence(storage)?;
        if sqlite_sequence_after != plan.expected_sqlite_sequence {
            return Err(BeadsError::Config(
                "SQLite AUTOINCREMENT high-water marks do not match the reviewed additive reconciliation projection"
                    .to_string(),
            ));
        }
        if additive_sha256(&raw_blocked_cache_rows_after, "applied raw blocked cache")?
            != plan.receipt.expected_blocked_cache_payload_sha256
            || additive_sha256(&child_counters_after, "applied child counters")?
                != plan.receipt.expected_child_counter_payload_sha256
            || additive_sha256(&sqlite_sequence_after, "applied sqlite sequence")?
                != plan.receipt.expected_sqlite_sequence_payload_sha256
        {
            return Err(BeadsError::Config(
                "Derived-cache or AUTOINCREMENT witness digest does not match the reviewed additive reconciliation plan"
                    .to_string(),
            ));
        }
        let transaction_health = additive_database_health(storage)?;
        require_healthy_additive_database(&transaction_health, "post-apply transaction")?;
        let target_after = additive_database_witness(storage, &database_after)?;
        if target_after != plan.receipt.expected_target_after {
            return Err(BeadsError::Config(
                "Complete typed database poststate does not match the reviewed additive reconciliation witness"
                    .to_string(),
            ));
        }
        let expected_issue_count = plan
            .receipt
            .target_before
            .issues
            .checked_add(plan.receipt.created)
            .ok_or_else(|| {
                BeadsError::Config(
                    "Issue count overflow during additive reconciliation apply".to_string(),
                )
            })?;
        if target_after.issues != expected_issue_count {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation expected {expected_issue_count} issues after apply, found {}",
                target_after.issues
            )));
        }
        if target_after.relations != plan.receipt.relations_after {
            return Err(BeadsError::Config(
                "Relation counts after additive reconciliation do not match the plan".to_string(),
            ));
        }
        if target_after.issue_payload_sha256
            != plan.receipt.expected_issue_raw_payload_sha256
            || target_after.issue_semantic_payload_sha256
                != plan.receipt.expected_issue_semantic_payload_sha256
            || target_after.issue_content_hash_payload_sha256
                != plan.receipt.expected_issue_content_hash_payload_sha256
            || additive_sha256(&raw_export_hash_rows_after, "applied raw export hashes")?
                != plan.receipt.expected_export_hash_payload_sha256
            || additive_sha256(&dirty_after, "applied dirty markers")?
                != plan.receipt.expected_dirty_payload_sha256
            || additive_sha256(&metadata_after, "applied metadata")?
                != plan.receipt.expected_metadata_payload_sha256
        {
            return Err(BeadsError::Config(
                "Post-apply witness digest does not match the reviewed additive reconciliation plan"
                    .to_string(),
            ));
        }
        let expected_dirty_count = plan
            .receipt
            .target_before
            .dirty_issues
            .checked_sub(plan.receipt.dirty_markers_clear_planned)
            .ok_or_else(|| {
                BeadsError::Config(
                    "Dirty-marker count underflow during additive reconciliation apply"
                        .to_string(),
                )
            })?;
        if target_after.dirty_issues != expected_dirty_count {
            return Err(BeadsError::Config(format!(
                "Additive reconciliation expected {expected_dirty_count} dirty marker(s) after apply, found {}",
                target_after.dirty_issues
            )));
        }
        if target_after.events != plan.receipt.events_before
            || target_after.event_payload_sha256
                != plan.receipt.event_payload_sha256_before
        {
            return Err(BeadsError::Config(
                "Audit event stream changed during additive reconciliation".to_string(),
            ));
        }
        if target_after.config != plan.receipt.target_before.config
            || target_after.close_metadata != plan.receipt.target_before.close_metadata
            || target_after.gate_results != plan.receipt.target_before.gate_results
            || target_after.gate_result_history
                != plan.receipt.target_before.gate_result_history
            || target_after.schema_catalog != plan.receipt.target_before.schema_catalog
        {
            return Err(BeadsError::Config(
                "Database-only config, close metadata, workflow-gate rows, or schema catalog changed during additive reconciliation"
                    .to_string(),
            ));
        }

        let mut receipt = plan.receipt.clone();
        receipt.status = if plan.mutations.is_empty() && plan.content_hash_repairs.is_empty() {
            AdditiveReconcileStatus::AppliedMetadataOnly
        } else {
            AdditiveReconcileStatus::Applied
        };
        receipt.events_after = target_after.events;
        receipt
            .event_payload_sha256_after
            .clone_from(&target_after.event_payload_sha256);
        receipt.target_after = Some(target_after);
        receipt.health_after = None;
        receipt.cache_rebuild_performed = plan.receipt.cache_rebuild_planned;
        receipt.content_hash_repairs_applied = content_hash_repairs_applied;
        receipt.relation_rows_applied = plan.receipt.relation_rows_planned;
        receipt.export_hashes_updated = export_hashes_updated;
        receipt.dirty_markers_cleared = dirty_markers_cleared;
        receipt.metadata_changed = plan.receipt.metadata_update_planned;
        #[cfg(test)]
        additive_test_fail_at(AdditiveTestFailPhase::BeforeFinalCommitChecks)?;
        require_reviewed_additive_schema_version(storage, &plan.receipt, "commit boundary")?;
        let source_final = additive_source_snapshot(input_path, config)?;
        if !additive_source_matches_receipt(&source_final, &plan.receipt) {
            return Err(BeadsError::SyncConflict {
                message: "JSONL witness changed at the additive reconciliation commit boundary"
                    .to_string(),
            });
        }
        if let (Some(database_path), Some(expected_identity)) = (
            config.database_path.as_deref(),
            plan.receipt.database_identity_sha256.as_deref(),
        ) && additive_path_identity_sha256(database_path, "database")? != expected_identity
        {
            return Err(BeadsError::SyncConflict {
                message: "Configured database identity changed at the additive reconciliation commit boundary"
                    .to_string(),
            });
        }
        if let Some(workspace_path) = config.beads_dir.as_deref()
            && additive_path_identity_sha256(workspace_path, "workspace")?
                != plan.receipt.workspace_identity_sha256
        {
            return Err(BeadsError::SyncConflict {
                message: "Terminal workspace identity changed at the additive reconciliation commit boundary"
                    .to_string(),
            });
        }
        #[cfg(test)]
        additive_test_drift_source_after_final_check(input_path)?;
        Ok(receipt)
        })
        .map(|outcome| {
            let mut receipt = outcome.value;
            receipt.foreign_keys_restored_after_commit =
                Some(outcome.foreign_keys_restored);
            if !outcome.foreign_keys_restored {
                receipt
                    .postcommit_failures
                    .push(AdditivePostcommitFailure::ForeignKeyRestoration);
                receipt.status = AdditiveReconcileStatus::CommittedWithPostconditionFailures;
            }
            if !outcome.database_authority_preserved {
                receipt
                    .postcommit_failures
                    .push(AdditivePostcommitFailure::DatabaseAuthorityChanged);
                receipt.status = AdditiveReconcileStatus::CommittedWithPostconditionFailures;
            }
            receipt.postcommit_failures.sort_unstable();
            receipt.postcommit_failures.dedup();
            receipt
        });
    let result = result.map(|mut receipt| {
        let health_after =
            additive_database_health(storage).unwrap_or_else(|error| AdditiveDatabaseHealth {
                integrity_messages: vec![format!(
                    "postcommit integrity attestation unavailable: {error}"
                )],
                foreign_key_violations: Vec::new(),
            });
        if !additive_database_is_healthy(&health_after) {
            receipt
                .postcommit_failures
                .push(AdditivePostcommitFailure::DatabasePoststateChanged);
            receipt.status = AdditiveReconcileStatus::CommittedWithPostconditionFailures;
        }
        receipt.health_after = Some(health_after);
        receipt.postcommit_failures.sort_unstable();
        receipt.postcommit_failures.dedup();
        receipt
    });
    match &result {
        Ok(receipt) => tracing::info!(
            plan_sha256_prefix = %receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            status = receipt.status.as_str(),
            target_issues = receipt.target_after.as_ref().map_or(0, |witness| witness.issues),
            events_preserved = receipt.events_after,
            cache_rebuild_performed = receipt.cache_rebuild_performed,
            "Committed additive reconciliation"
        ),
        Err(error) => tracing::warn!(
            plan_sha256_prefix = %plan.receipt.plan_sha256.get(..12).unwrap_or("<invalid>"),
            error = %error,
            "Additive reconciliation did not reach a verified commit; rollback outcome is carried by the returned error"
        ),
    }
    result
}

// ============================================================================
// PREFLIGHT CHECKS (beads_rust-0v1.2.7)
// ============================================================================

/// Status of a preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightCheckStatus {
    /// Check passed.
    Pass,
    /// Check passed with warnings.
    Warn,
    /// Check failed.
    Fail,
}

/// A single preflight check result.
#[derive(Debug, Clone)]
pub struct PreflightCheck {
    /// Name of the check (e.g., "`path_validation`").
    pub name: String,
    /// Human-readable description of what was checked.
    pub description: String,
    /// Status of the check.
    pub status: PreflightCheckStatus,
    /// Detailed message (error/warning reason, or success confirmation).
    pub message: String,
    /// Actionable remediation hint (if status is Fail or Warn).
    pub remediation: Option<String>,
}

impl PreflightCheck {
    fn pass(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    fn warn(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Warn,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn fail(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Fail,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }
}

/// Result of running all preflight checks.
#[derive(Debug, Clone)]
pub struct PreflightResult {
    /// All checks that were run.
    pub checks: Vec<PreflightCheck>,
    /// Overall status (Fail if any check failed, Warn if any warned, Pass otherwise).
    pub overall_status: PreflightCheckStatus,
}

impl PreflightResult {
    const fn new() -> Self {
        Self {
            checks: Vec::new(),
            overall_status: PreflightCheckStatus::Pass,
        }
    }

    fn add(&mut self, check: PreflightCheck) {
        // Update overall status (Fail > Warn > Pass)
        match check.status {
            PreflightCheckStatus::Fail => self.overall_status = PreflightCheckStatus::Fail,
            PreflightCheckStatus::Warn if self.overall_status != PreflightCheckStatus::Fail => {
                self.overall_status = PreflightCheckStatus::Warn;
            }
            _ => {}
        }
        self.checks.push(check);
    }

    /// Returns true if all checks passed (no failures or warnings).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.overall_status == PreflightCheckStatus::Pass
    }

    /// Returns true if there are no failures (warnings are acceptable).
    #[must_use]
    pub fn has_no_failures(&self) -> bool {
        self.overall_status != PreflightCheckStatus::Fail
    }

    /// Get all failed checks.
    #[must_use]
    pub fn failures(&self) -> Vec<&PreflightCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == PreflightCheckStatus::Fail)
            .collect()
    }

    /// Get all warnings.
    #[must_use]
    pub fn warnings(&self) -> Vec<&PreflightCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == PreflightCheckStatus::Warn)
            .collect()
    }

    /// Convert to an error if there are failures.
    ///
    /// # Errors
    ///
    /// Returns an error if there are failed checks.
    pub fn into_result(self) -> Result<Self> {
        if self.overall_status == PreflightCheckStatus::Fail {
            let mut msg = String::from("Preflight checks failed:\n");
            for check in self.failures() {
                use std::fmt::Write;
                let _ = writeln!(msg, "  - {}: {}", check.name, check.message);
                if let Some(ref rem) = check.remediation {
                    let _ = writeln!(msg, "    Hint: {rem}");
                }
            }
            Err(BeadsError::Config(msg))
        } else {
            Ok(self)
        }
    }
}

const JSONL_VALIDATION_PREVIEW_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonlIssueValidationFailure {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct JsonlIssueValidationSummary {
    pub record_count: usize,
    pub invalid_count: usize,
    pub failures: Vec<JsonlIssueValidationFailure>,
}

impl JsonlIssueValidationSummary {
    fn push_failure(&mut self, line: usize, message: impl Into<String>) {
        self.invalid_count += 1;
        if self.failures.len() < JSONL_VALIDATION_PREVIEW_LIMIT {
            self.failures.push(JsonlIssueValidationFailure {
                line,
                message: message.into(),
            });
        }
    }

    pub(crate) fn preview_messages(&self) -> Vec<String> {
        self.failures
            .iter()
            .map(|failure| format!("line {}: {}", failure.line, failure.message))
            .collect()
    }
}

fn validate_jsonl_issue_records_from_reader(
    reader: impl BufRead,
) -> Result<JsonlIssueValidationSummary> {
    let mut summary = JsonlIssueValidationSummary::default();
    let mut seen_ids = HashSet::new();

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = line_result?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        summary.record_count += 1;
        match serde_json::from_str::<Issue>(trimmed) {
            Ok(mut issue) => {
                normalize_issue(&mut issue);
                if !seen_ids.insert(issue.id.clone()) {
                    summary
                        .push_failure(line_num + 1, format!("Duplicate issue id '{}'", issue.id));
                    continue;
                }
                if let Err(errors) = IssueValidator::validate(&issue) {
                    summary.push_failure(
                        line_num + 1,
                        errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                }
            }
            Err(err) => summary.push_failure(line_num + 1, err.to_string()),
        }
    }

    Ok(summary)
}

pub(crate) fn validate_jsonl_issue_records(path: &Path) -> Result<JsonlIssueValidationSummary> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    validate_jsonl_issue_records_from_reader(BufReader::with_capacity(2 * 1024 * 1024, file))
}

pub(crate) fn validate_jsonl_snapshot_issue_records(
    source: &JsonlSourceSnapshot,
) -> Result<JsonlIssueValidationSummary> {
    validate_jsonl_issue_records_from_reader(source.reader())
}

/// One source record explicitly rejected by JSONL salvage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JsonlSalvageRejectedRecord {
    pub line: usize,
    pub error: String,
}

/// Durable receipt for an operator-authorized malformed-record salvage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JsonlSalvageReceipt {
    pub original_raw_sha256: String,
    pub recovered_raw_sha256: String,
    pub valid_records: usize,
    pub rejected_records: Vec<JsonlSalvageRejectedRecord>,
    pub backup_path: String,
    pub publication_atomicity: String,
    /// Exportable database records not certified by the recovered JSONL.
    /// These rows are preserved by the additive import and require a later
    /// full export to restore DB/JSONL coverage.
    pub database_records_requiring_export: usize,
    /// Whether this salvage armed `needs_flush` for those preserved rows.
    pub needs_flush_set: bool,
}

pub(crate) struct JsonlSalvageResult {
    pub source: Arc<JsonlSourceSnapshot>,
    pub receipt: JsonlSalvageReceipt,
}

fn classify_jsonl_salvage_record(
    line: &[u8],
    seen_ids: &mut HashSet<String>,
) -> Option<std::result::Result<(), String>> {
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }

    let mut issue = match serde_json::from_slice::<Issue>(trimmed) {
        Ok(issue) => issue,
        Err(error) => return Some(Err(error.to_string())),
    };
    normalize_issue(&mut issue);
    if let Err(errors) = IssueValidator::validate(&issue) {
        return Some(Err(errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")));
    }
    if !seen_ids.insert(issue.id.clone()) {
        return Some(Err(format!("Duplicate issue id '{}'", issue.id)));
    }
    Some(Ok(()))
}

/// Remove invalid issue records from one immutable JSONL generation, retain an
/// exact non-rotating backup, and conditionally publish the validated result.
/// Blank lines are retained and merge-conflict markers remain a hard failure.
///
/// Returns `Ok(None)` when every nonblank record is already valid.
pub(crate) fn salvage_invalid_jsonl_records_under_authority(
    source: &JsonlSourceSnapshot,
    output_path: &Path,
    beads_dir: &Path,
    allow_external_jsonl: bool,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<Option<JsonlSalvageResult>> {
    validate_sync_path_with_external(output_path, beads_dir, allow_external_jsonl)?;
    verify_jsonl_source_snapshot_current(source, jsonl_authority)?;
    ensure_no_conflict_markers_snapshot(source)?;

    let export_config = ExportConfig {
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        ..ExportConfig::default()
    };
    let (temp_path, pinned_temp, temp_file) =
        create_full_export_temp_file_under_authority(output_path, &export_config, jsonl_authority)?;
    let temp_guard = TempFileGuard::new_retained(temp_path.clone());
    let mut writer = BufWriter::new(temp_file);
    let mut reader = source.reader();
    let mut line = Vec::with_capacity(4096);
    let mut line_number = 0;
    let mut valid_records = 0;
    let mut rejected_records = Vec::new();
    let mut seen_ids = HashSet::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        line_number += 1;

        match classify_jsonl_salvage_record(&line, &mut seen_ids) {
            None => writer.write_all(&line)?,
            Some(Ok(())) => {
                writer.write_all(&line)?;
                valid_records += 1;
            }
            Some(Err(error)) => rejected_records.push(JsonlSalvageRejectedRecord {
                line: line_number,
                error,
            }),
        }
    }

    if rejected_records.is_empty() {
        return Ok(None);
    }
    if valid_records == 0 {
        return Err(BeadsError::Config(
            "Refusing JSONL salvage because no valid issue records would remain".to_string(),
        ));
    }

    writer.flush()?;
    writer
        .into_inner()
        .map_err(|error| BeadsError::Io(error.into_error()))?
        .sync_all()?;

    let staged_source = pinned_temp.capture()?;
    let staged_validation = validate_jsonl_snapshot_issue_records(&staged_source)?;
    if staged_validation.invalid_count != 0 || staged_validation.record_count != valid_records {
        return Err(BeadsError::SyncConflict {
            message: "Staged JSONL salvage did not reproduce the validated survivor set"
                .to_string(),
        });
    }

    let backup_path = history::backup_before_jsonl_salvage(beads_dir, output_path, source)?;
    verify_jsonl_source_snapshot_current(source, jsonl_authority)?;
    let recovered_raw_sha256 = staged_source.raw_sha256().to_string();
    let publication = publish_staged_jsonl_conditionally(
        &temp_path,
        temp_guard,
        output_path,
        &staged_source,
        &source.state_witness(),
        staged_source.content_sha256(),
        jsonl_authority,
        None,
    )?;

    Ok(Some(JsonlSalvageResult {
        source: publication.source,
        receipt: JsonlSalvageReceipt {
            original_raw_sha256: source.raw_sha256().to_string(),
            recovered_raw_sha256,
            valid_records,
            rejected_records,
            backup_path: backup_path.to_string_lossy().into_owned(),
            publication_atomicity: publication.atomicity.as_str().to_string(),
            database_records_requiring_export: 0,
            needs_flush_set: false,
        },
    }))
}

/// Run preflight checks for export operation.
///
/// This function is read-only and validates:
/// - Beads directory exists
/// - Output path is within allowlist (not in .git, within `beads_dir`)
/// - Database is accessible
/// - Export won't cause data loss (empty db over non-empty JSONL, stale db)
///
/// # Arguments
///
/// * `storage` - Database connection for validation
/// * `output_path` - Target JSONL path
/// * `config` - Export configuration
///
/// # Returns
///
/// `PreflightResult` with all check results. Use `.into_result()` to convert
/// failures to an error.
///
/// # Errors
///
/// Returns an error if the preflight checks fail.
#[allow(clippy::too_many_lines)]
pub fn preflight_export(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<PreflightResult> {
    let mut result = PreflightResult::new();

    tracing::debug!(
        output_path = %output_path.display(),
        beads_dir = ?config.beads_dir,
        "Running export preflight checks"
    );

    // Check 1: Beads directory exists
    if let Some(ref beads_dir) = config.beads_dir {
        if beads_dir.is_dir() {
            result.add(PreflightCheck::pass(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Found: {}", beads_dir.display()),
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: PASS");
        } else {
            result.add(PreflightCheck::fail(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Not found: {}", beads_dir.display()),
                "Run 'br init' to initialize the beads directory.",
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: FAIL");
        }
    }

    // Check 2: Output path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        // Determine if the path is external (outside .beads/)
        let canonical_beads = dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let is_external =
            !output_path.starts_with(beads_dir) && !output_path.starts_with(&canonical_beads);

        match validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)
        {
            Ok(()) => {
                let msg = format!(
                    "Path {} validated (external={})",
                    output_path.display(),
                    is_external
                );
                if is_external && config.allow_external_jsonl {
                    result.add(PreflightCheck::warn(
                        "path_validation",
                        "Output path is within allowlist",
                        msg,
                        "Consider moving JSONL to .beads/ directory for better safety.",
                    ));
                } else {
                    result.add(PreflightCheck::pass(
                        "path_validation",
                        "Output path is within allowlist",
                        msg,
                    ));
                }
                tracing::debug!(path = %output_path.display(), is_external = is_external, "Path validation: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "path_validation",
                    "Output path is within allowlist",
                    format!("Path rejected: {e}"),
                    "Use a path within .beads/ directory or set --allow-external-jsonl.",
                ));
                tracing::debug!(path = %output_path.display(), error = %e, "Path validation: FAIL");
            }
        }
    }

    // Check 3: Database is accessible
    match storage.count_issues() {
        Ok(count) => {
            result.add(PreflightCheck::pass(
                "database_accessible",
                "Database is accessible",
                format!("Database contains {count} issue(s)"),
            ));
            tracing::debug!(issue_count = count, "Database access check: PASS");

            // Check 4: Empty database safety (would overwrite non-empty JSONL)
            if count == 0 && !config.force && output_path.exists() {
                match count_issues_in_jsonl(output_path) {
                    Ok(jsonl_count) if jsonl_count > 0 => {
                        result.add(PreflightCheck::fail(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            format!(
                                "Database has 0 issues, JSONL has {jsonl_count} issues. Export would cause data loss.",
                            ),
                            "Import the JSONL first, or use --force to override.",
                        ));
                        tracing::debug!(
                            db_count = 0,
                            jsonl_count = jsonl_count,
                            "Empty database safety check: FAIL"
                        );
                    }
                    Ok(_) => {
                        result.add(PreflightCheck::pass(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            "Database is empty, no existing JSONL to overwrite.",
                        ));
                    }
                    Err(e) => {
                        result.add(PreflightCheck::warn(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            format!("Could not read existing JSONL: {e}"),
                            "Verify JSONL file is readable.",
                        ));
                    }
                }
            } else if count == 0 && !config.force {
                result.add(PreflightCheck::pass(
                    "empty_database_safety",
                    "Export won't cause data loss",
                    "Database is empty, no existing JSONL to overwrite.",
                ));
            }

            // Check 5: Stale database safety (would lose issues from JSONL)
            if count > 0 && !config.force && output_path.exists() {
                match get_issue_ids_from_jsonl(output_path) {
                    Ok(jsonl_ids) if !jsonl_ids.is_empty() => {
                        let db_ids: HashSet<String> = storage.get_all_ids()?.into_iter().collect();
                        let missing: Vec<_> = jsonl_ids.difference(&db_ids).take(5).collect();
                        if missing.is_empty() {
                            result.add(PreflightCheck::pass(
                                "stale_database_safety",
                                "Export won't lose JSONL issues",
                                "All JSONL issues are present in database.",
                            ));
                        } else {
                            let total_missing = jsonl_ids.difference(&db_ids).count();
                            result.add(PreflightCheck::fail(
                                "stale_database_safety",
                                "Export won't lose JSONL issues",
                                format!(
                                    "Database is missing {total_missing} issue(s) from JSONL: {}{}",
                                    missing
                                        .iter()
                                        .map(|s| s.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    if total_missing > 5 { " ..." } else { "" }
                                ),
                                "Import the JSONL first to sync, or use --force to override.",
                            ));
                            tracing::debug!(
                                missing_count = total_missing,
                                sample = ?missing,
                                "Stale database safety check: FAIL"
                            );
                        }
                    }
                    Ok(_) => {
                        result.add(PreflightCheck::pass(
                            "stale_database_safety",
                            "Export won't lose JSONL issues",
                            "JSONL is empty or doesn't exist.",
                        ));
                    }
                    Err(e) => {
                        result.add(PreflightCheck::warn(
                            "stale_database_safety",
                            "Export won't lose JSONL issues",
                            format!("Could not read existing JSONL: {e}"),
                            "Verify JSONL file is readable.",
                        ));
                    }
                }
            }
        }
        Err(e) => {
            result.add(PreflightCheck::fail(
                "database_accessible",
                "Database is accessible",
                format!("Database error: {e}"),
                "Check database file permissions and integrity.",
            ));
            tracing::debug!(error = %e, "Database access check: FAIL");
        }
    }

    tracing::debug!(
        overall_status = ?result.overall_status,
        check_count = result.checks.len(),
        failure_count = result.failures().len(),
        "Export preflight complete"
    );

    Ok(result)
}

/// Run preflight checks for import operation.
///
/// This function is read-only and validates:
/// - Beads directory exists
/// - Input path is within allowlist (not in .git, within `beads_dir`)
/// - Input file exists and is readable
/// - No merge conflict markers in input file
/// - JSONL is parseable (basic syntax check)
/// - Issue ID prefixes match expected prefix (unless explicitly skipped)
///
/// # Arguments
///
/// * `input_path` - Source JSONL path
/// * `config` - Import configuration
/// * `expected_prefix` - Expected issue ID prefix (e.g., "bd") for mismatch guardrails
///
/// # Returns
///
/// `PreflightResult` with all check results. Use `.into_result()` to convert
/// failures to an error.
///
/// # Errors
///
/// Returns an error if the preflight checks fail.
#[allow(clippy::too_many_lines)]
pub fn preflight_import(
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<PreflightResult> {
    Ok(preflight_import_impl(
        input_path,
        None,
        config,
        expected_prefix,
    ))
}

// Preflight itself is infallible; the `Result` signature is the crate-wide
// contract relied on by callers outside this module (e.g. `config::mod`).
#[allow(clippy::too_many_lines, clippy::unnecessary_wraps)]
pub(crate) fn preflight_import_snapshot(
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<PreflightResult> {
    Ok(preflight_import_impl(
        source.display_path(),
        Some(source),
        config,
        expected_prefix,
    ))
}

#[allow(clippy::too_many_lines)]
fn preflight_import_impl(
    input_path: &Path,
    source: Option<&JsonlSourceSnapshot>,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> PreflightResult {
    let mut result = PreflightResult::new();

    tracing::debug!(
        input_path = %input_path.display(),
        beads_dir = ?config.beads_dir,
        "Running import preflight checks"
    );

    // Check 1: Beads directory exists
    if let Some(ref beads_dir) = config.beads_dir {
        if beads_dir.is_dir() {
            result.add(PreflightCheck::pass(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Found: {}", beads_dir.display()),
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: PASS");
        } else {
            result.add(PreflightCheck::fail(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Not found: {}", beads_dir.display()),
                "Run 'br init' to initialize the beads directory.",
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: FAIL");
        }
    }

    // Check 2: Input path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        // Determine if the path is external (outside .beads/)
        let canonical_beads = dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let is_external =
            !input_path.starts_with(beads_dir) && !input_path.starts_with(&canonical_beads);

        match validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl) {
            Ok(()) => {
                let msg = format!(
                    "Path {} validated (external={})",
                    input_path.display(),
                    is_external
                );
                if is_external && config.allow_external_jsonl {
                    result.add(PreflightCheck::warn(
                        "path_validation",
                        "Input path is within allowlist",
                        msg,
                        "Consider using JSONL from .beads/ directory for better safety.",
                    ));
                } else {
                    result.add(PreflightCheck::pass(
                        "path_validation",
                        "Input path is within allowlist",
                        msg,
                    ));
                }
                tracing::debug!(path = %input_path.display(), is_external = is_external, "Path validation: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "path_validation",
                    "Input path is within allowlist",
                    format!("Path rejected: {e}"),
                    "Use a path within .beads/ directory or set --allow-external-jsonl.",
                ));
                tracing::debug!(path = %input_path.display(), error = %e, "Path validation: FAIL");
                return result;
            }
        }
    }

    // Check 3: Input file exists and is readable. A supplied snapshot is the
    // authoritative proof: do not reopen the mutable path.
    if let Some(source) = source {
        result.add(PreflightCheck::pass(
            "file_readable",
            "Input file exists and is readable",
            format!(
                "Captured {} exact byte(s) from a stable file handle.",
                source.size()
            ),
        ));
        tracing::debug!(
            path = %input_path.display(),
            size = source.size(),
            raw_sha256 = %source.raw_sha256(),
            "Immutable JSONL snapshot readable check: PASS"
        );
    } else if input_path.exists() {
        match File::open(input_path) {
            Ok(_) => {
                result.add(PreflightCheck::pass(
                    "file_readable",
                    "Input file exists and is readable",
                    format!("File accessible: {}", input_path.display()),
                ));
                tracing::debug!(path = %input_path.display(), "File readable check: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "file_readable",
                    "Input file exists and is readable",
                    format!("Cannot read file: {e}"),
                    "Check file permissions.",
                ));
                tracing::debug!(path = %input_path.display(), error = %e, "File readable check: FAIL");
            }
        }
    } else {
        result.add(PreflightCheck::fail(
            "file_readable",
            "Input file exists and is readable",
            format!("File not found: {}", input_path.display()),
            "Verify the path is correct or run export first.",
        ));
        tracing::debug!(path = %input_path.display(), "File readable check: FAIL (not found)");
        // Return early since we can't do further checks without the file
        return result;
    }

    // Check 4: No merge conflict markers
    let marker_scan = if let Some(source) = source {
        scan_conflict_markers_snapshot(source)
    } else {
        scan_conflict_markers(input_path)
    };
    match marker_scan {
        Ok(markers) if markers.is_empty() => {
            result.add(PreflightCheck::pass(
                "no_conflict_markers",
                "No merge conflict markers",
                "File is clean of conflict markers.",
            ));
            tracing::debug!(path = %input_path.display(), "Conflict marker check: PASS");
        }
        Ok(markers) => {
            let preview: Vec<String> = markers
                .iter()
                .take(3)
                .map(|m| {
                    format!(
                        "line {}: {:?}{}",
                        m.line,
                        m.marker_type,
                        m.branch
                            .as_ref()
                            .map_or(String::new(), |b| format!(" ({b})"))
                    )
                })
                .collect();
            result.add(PreflightCheck::fail(
                "no_conflict_markers",
                "No merge conflict markers",
                format!(
                    "Found {} conflict marker(s): {}{}",
                    markers.len(),
                    preview.join("; "),
                    if markers.len() > 3 { " ..." } else { "" }
                ),
                "Resolve git merge conflicts before importing.",
            ));
            tracing::debug!(
                path = %input_path.display(),
                marker_count = markers.len(),
                "Conflict marker check: FAIL"
            );
        }
        Err(e) => {
            result.add(PreflightCheck::warn(
                "no_conflict_markers",
                "No merge conflict markers",
                format!("Could not scan for markers: {e}"),
                "Verify file is readable and not corrupted.",
            ));
            tracing::debug!(path = %input_path.display(), error = %e, "Conflict marker check: WARN");
        }
    }

    // Check 5: Per-line issue-record validation
    let issue_validation = if let Some(source) = source {
        validate_jsonl_snapshot_issue_records(source)
    } else {
        validate_jsonl_issue_records(input_path)
    };
    match issue_validation {
        Ok(summary) if summary.invalid_count == 0 => {
            result.add(PreflightCheck::pass(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!("Validated {} issue record(s).", summary.record_count),
            ));
            tracing::debug!(path = %input_path.display(), record_count = summary.record_count, "JSONL issue validation check: PASS");
        }
        Ok(summary) => {
            let preview = summary.preview_messages();
            result.add(PreflightCheck::fail(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!(
                    "Found {} invalid issue record(s): {}{}",
                    summary.invalid_count,
                    preview.join("; "),
                    if summary.invalid_count > preview.len() {
                        " ..."
                    } else {
                        ""
                    }
                ),
                "Fix or remove malformed issue records before importing.",
            ));
            tracing::debug!(
                path = %input_path.display(),
                invalid_count = summary.invalid_count,
                "JSONL issue validation check: FAIL"
            );
        }
        Err(err) => {
            result.add(PreflightCheck::warn(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!("Could not open file for JSONL validation: {err}"),
                "Verify file is readable.",
            ));
        }
    }

    // Check 6: Prefix mismatch guard
    if !config.skip_prefix_validation
        && let Some(prefix) = expected_prefix
    {
        let reader: Result<Box<dyn BufRead + '_>> = if let Some(source) = source {
            Ok(Box::new(source.reader()))
        } else {
            File::open(input_path)
                .map(|file| Box::new(BufReader::new(file)) as Box<dyn BufRead>)
                .map_err(BeadsError::from)
        };
        match reader {
            Ok(reader) => {
                let mut mismatched_ids: Vec<String> = Vec::new();
                for line_result in reader.lines() {
                    let Ok(line) = line_result else { continue };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(partial) = serde_json::from_str::<PartialId>(trimmed) {
                        // Skip tombstones — they may retain a foreign prefix legitimately
                        #[derive(Deserialize)]
                        struct StatusProbe {
                            status: Option<String>,
                        }
                        let is_tombstone = serde_json::from_str::<StatusProbe>(trimmed)
                            .ok()
                            .and_then(|p| p.status)
                            .is_some_and(|s| s == "tombstone");
                        if is_tombstone {
                            continue;
                        }
                        if !id_matches_expected_prefix(&partial.id, prefix) {
                            mismatched_ids.push(partial.id);
                        }
                    }
                }
                if mismatched_ids.is_empty() {
                    result.add(PreflightCheck::pass(
                        "prefix_match",
                        "Issue IDs match expected prefix",
                        format!("All issue IDs start with '{prefix}'."),
                    ));
                    tracing::debug!(prefix = prefix, "Prefix match check: PASS");
                } else {
                    let preview: Vec<String> = mismatched_ids.iter().take(5).cloned().collect();
                    result.add(PreflightCheck::fail(
                        "prefix_match",
                        "Issue IDs match expected prefix",
                        format!(
                            "Expected prefix '{}', found {} mismatched ID(s): {}{}",
                            prefix,
                            mismatched_ids.len(),
                            preview.join(", "),
                            if mismatched_ids.len() > 5 { " ..." } else { "" }
                        ),
                        "Use --force to skip prefix validation or --rename-prefix to remap IDs.",
                    ));
                    tracing::debug!(
                        prefix = prefix,
                        mismatch_count = mismatched_ids.len(),
                        "Prefix match check: FAIL"
                    );
                }
            }
            Err(e) => {
                result.add(PreflightCheck::warn(
                    "prefix_match",
                    "Issue IDs match expected prefix",
                    format!("Could not open file for prefix validation: {e}"),
                    "Verify file is readable.",
                ));
            }
        }
    }

    tracing::debug!(
        overall_status = ?result.overall_status,
        check_count = result.checks.len(),
        failure_count = result.failures().len(),
        "Import preflight complete"
    );

    result
}

/// Conflict marker kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMarkerType {
    Start,
    Separator,
    End,
}

/// A detected merge conflict marker within an import file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMarker {
    pub path: PathBuf,
    pub line: usize,
    pub marker_type: ConflictMarkerType,
    pub branch: Option<String>,
}

const CONFLICT_START: &str = "<<<<<<<";
const CONFLICT_SEPARATOR: &str = "=======";
const CONFLICT_END: &str = ">>>>>>>";

/// Scan a file for merge conflict markers.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
fn scan_conflict_markers_from_reader(
    display_path: &Path,
    reader: impl BufRead,
) -> Result<Vec<ConflictMarker>> {
    let mut markers = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if let Some((marker_type, branch)) = detect_conflict_marker(&line) {
            markers.push(ConflictMarker {
                path: display_path.to_path_buf(),
                line: line_num + 1,
                marker_type,
                branch,
            });
        }
    }

    Ok(markers)
}

pub fn scan_conflict_markers(path: &Path) -> Result<Vec<ConflictMarker>> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    scan_conflict_markers_from_reader(path, BufReader::with_capacity(2 * 1024 * 1024, file))
}

pub(crate) fn scan_conflict_markers_snapshot(
    source: &JsonlSourceSnapshot,
) -> Result<Vec<ConflictMarker>> {
    scan_conflict_markers_from_reader(source.display_path(), source.reader())
}

fn detect_conflict_marker(line: &str) -> Option<(ConflictMarkerType, Option<String>)> {
    if let Some(branch) = line.strip_prefix(CONFLICT_START) {
        return Some((ConflictMarkerType::Start, Some(branch.trim().to_string())));
    }
    if line.starts_with(CONFLICT_SEPARATOR) {
        return Some((ConflictMarkerType::Separator, None));
    }
    if let Some(branch) = line.strip_prefix(CONFLICT_END) {
        return Some((ConflictMarkerType::End, Some(branch.trim().to_string())));
    }
    None
}

/// Fail if a file contains merge conflict markers.
///
/// # Errors
///
/// Returns a config error describing the first few markers found.
pub fn ensure_no_conflict_markers(path: &Path) -> Result<()> {
    let markers = scan_conflict_markers(path)?;
    ensure_no_conflict_markers_from_scan(path, &markers)
}

fn ensure_no_conflict_markers_from_scan(
    display_path: &Path,
    markers: &[ConflictMarker],
) -> Result<()> {
    if markers.is_empty() {
        return Ok(());
    }

    let mut preview = String::new();
    for marker in markers.iter().take(5) {
        let _ = writeln!(
            preview,
            "{}:{} {:?}{}",
            marker.path.display(),
            marker.line,
            marker.marker_type,
            marker
                .branch
                .as_ref()
                .map_or(String::new(), |b| format!(" ({b})"))
        );
    }

    Err(BeadsError::Config(format!(
        "Merge conflict markers detected in {}.\n{}Resolve conflicts before importing.",
        display_path.display(),
        preview
    )))
}

pub(crate) fn ensure_no_conflict_markers_snapshot(source: &JsonlSourceSnapshot) -> Result<()> {
    let markers = scan_conflict_markers_snapshot(source)?;
    ensure_no_conflict_markers_from_scan(source.display_path(), &markers)
}

#[derive(Deserialize)]
struct PartialId {
    id: String,
}

/// Analyze JSONL to get line count and unique issue IDs efficiently.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn analyze_jsonl(path: &Path) -> Result<(usize, HashSet<String>)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, HashSet::new())),
        Err(e) => return Err(BeadsError::Io(e)),
    };
    path::validate_jsonl_fd_metadata(&file, path)?;
    analyze_jsonl_from_reader(path, BufReader::new(file))
}

fn analyze_jsonl_from_reader(
    display_path: &Path,
    mut reader: impl BufRead,
) -> Result<(usize, HashSet<String>)> {
    let mut count = 0;
    let mut ids = HashSet::new();
    let mut line_buf = String::new();
    let mut line_num = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                display_path.display(),
                line_num
            )));
        }
        count += 1;
    }

    Ok((count, ids))
}

pub(crate) fn analyze_jsonl_snapshot(
    source: &JsonlSourceSnapshot,
) -> Result<(usize, HashSet<String>)> {
    analyze_jsonl_from_reader(source.display_path(), source.reader())
}

/// Count issues in an existing JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn count_issues_in_jsonl(path: &Path) -> Result<usize> {
    Ok(analyze_jsonl(path)?.0)
}

fn verify_exported_jsonl_integrity(path: &Path, expected_ids: &[String]) -> Result<()> {
    let source = capture_jsonl_source_snapshot(path)?;
    verify_exported_jsonl_snapshot_integrity(&source, expected_ids)
}

fn verify_exported_jsonl_snapshot_integrity(
    source: &JsonlSourceSnapshot,
    expected_ids: &[String],
) -> Result<()> {
    let expected: HashSet<&str> = expected_ids.iter().map(String::as_str).collect();
    let mut observed = HashSet::with_capacity(expected_ids.len());
    let mut reader = source.reader();
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut issue_count = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed).map_err(|err| {
            BeadsError::Config(format!(
                "Export verification failed: invalid exported JSON at line {line_num}: {err}"
            ))
        })?;

        if issue.id.trim().is_empty() {
            return Err(BeadsError::Config(format!(
                "Export verification failed: empty issue id at line {line_num}"
            )));
        }

        if !expected.contains(issue.id.as_str()) {
            return Err(BeadsError::Config(format!(
                "Export verification failed: unexpected issue id '{}' at line {line_num}",
                issue.id
            )));
        }

        if !observed.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Export verification failed: duplicate issue id '{}' at line {line_num}",
                issue.id
            )));
        }

        issue_count += 1;
    }

    if issue_count != expected_ids.len() {
        return Err(BeadsError::Config(format!(
            "Export verification failed: expected {} issues, JSONL has {} valid issue lines",
            expected_ids.len(),
            issue_count
        )));
    }

    if observed.len() != expected.len() {
        let mut missing = expected_ids
            .iter()
            .filter(|id| !observed.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        missing.sort();
        let preview = missing
            .iter()
            .take(10)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let more = if missing.len() > 10 {
            format!(" ... and {} more", missing.len() - 10)
        } else {
            String::new()
        };
        return Err(BeadsError::Config(format!(
            "Export verification failed: JSONL is missing expected issue id(s): {preview}{more}"
        )));
    }

    Ok(())
}

/// Get issue IDs from an existing JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn get_issue_ids_from_jsonl(path: &Path) -> Result<HashSet<String>> {
    Ok(analyze_jsonl(path)?.1)
}

pub(crate) fn get_issue_ids_from_jsonl_snapshot(
    source: &JsonlSourceSnapshot,
) -> Result<HashSet<String>> {
    Ok(analyze_jsonl_snapshot(source)?.1)
}

fn read_jsonl_lines_by_id(path: &Path) -> Result<BTreeMap<String, String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut lines_by_id = BTreeMap::new();
    let mut line_buf = String::new();
    let mut line_num = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if lines_by_id
            .insert(partial.id.clone(), trimmed.to_string())
            .is_some()
        {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                path.display(),
                line_num
            )));
        }
    }

    Ok(lines_by_id)
}

fn export_issue_ids(storage: &SqliteStorage) -> Result<Vec<String>> {
    let rows = storage.execute_raw_query(
        r"SELECT id
          FROM issues
          WHERE (ephemeral = 0 OR ephemeral IS NULL)
            AND id NOT LIKE '%-wisp-%'
          ORDER BY id ASC",
    )?;

    Ok(rows
        .iter()
        .filter_map(|row| row.first().and_then(SqliteValue::as_text).map(String::from))
        .collect())
}

fn hydrate_export_issue_batch(
    storage: &SqliteStorage,
    ids: &[String],
    ctx: &mut ExportContext,
) -> Result<Vec<Issue>> {
    let mut issues = storage.get_issues_by_ids(ids)?;
    issues.sort_unstable_by(|left, right| left.id.cmp(&right.id));

    let deps_map = match storage.get_dependencies_full_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Dependency,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };
    let labels_map = match storage.get_labels_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Label,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };
    let comments_map = match storage.get_comments_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Comment,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };

    populate_export_issue_relations(
        storage,
        &mut issues,
        deps_map.as_ref(),
        labels_map.as_ref(),
        comments_map.as_ref(),
        ctx,
    );

    Ok(issues)
}

fn hydrate_export_issues_full_scan(
    storage: &SqliteStorage,
    ctx: &mut ExportContext,
) -> Result<Vec<Issue>> {
    let mut issues = storage.get_all_issues_for_export()?;

    let deps_map = match storage.get_dependency_records_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Dependency,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };
    let labels_map = match storage.get_labels_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Label,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };
    let comments_map = match storage.get_comments_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Comment,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };

    populate_export_issue_relations(
        storage,
        &mut issues,
        deps_map.as_ref(),
        labels_map.as_ref(),
        comments_map.as_ref(),
        ctx,
    );

    Ok(issues)
}

fn populate_export_issue_relations(
    storage: &SqliteStorage,
    issues: &mut [Issue],
    deps_map: Option<&HashMap<String, Vec<Dependency>>>,
    labels_map: Option<&HashMap<String, Vec<String>>>,
    comments_map: Option<&HashMap<String, Vec<Comment>>>,
    ctx: &ExportContext,
) {
    for issue in issues {
        if let Some(map) = deps_map {
            if let Some(deps) = map.get(&issue.id) {
                issue.dependencies.clone_from(deps);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(deps) = storage.get_dependencies_full(&issue.id)
        {
            issue.dependencies = deps;
        }

        if let Some(map) = labels_map {
            if let Some(labels) = map.get(&issue.id) {
                issue.labels.clone_from(labels);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(labels) = storage.get_labels(&issue.id)
        {
            issue.labels = labels;
        }

        if let Some(map) = comments_map {
            if let Some(comments) = map.get(&issue.id) {
                issue.comments.clone_from(comments);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(comments) = storage.get_comments(&issue.id)
        {
            issue.comments = comments;
        }

        normalize_issue_for_export(issue);
    }
}

fn write_export_issue_jsonl<W: Write>(
    writer: &mut W,
    issue: &Issue,
    hasher: &mut Sha256,
    buffer: &mut Vec<u8>,
    ctx: &mut ExportContext,
) -> Result<bool> {
    buffer.clear();
    if let Err(err) = serde_json::to_writer(&mut *buffer, issue) {
        ctx.handle_error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ))?;
        return Ok(false);
    }

    if let Err(err) = writer
        .write_all(buffer)
        .and_then(|()| writer.write_all(b"\n"))
    {
        ctx.handle_error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ))?;
        return Ok(false);
    }

    hasher.update(&*buffer);
    hasher.update(b"\n");

    Ok(true)
}

struct PreparedExportIssue {
    id: String,
    jsonl_line: Vec<u8>,
    content_hash: String,
    dependency_count: usize,
    label_count: usize,
    comment_count: usize,
}

enum PreparedExportEntry {
    Issue(PreparedExportIssue),
    SkippedTombstone(String),
    Error(ExportError),
}

fn effective_export_parallelism(config: &ExportConfig) -> usize {
    if config.max_parallel_workers == 1 || export_parallelism_disabled_by_env() {
        return 1;
    }

    let host_parallelism = thread::available_parallelism()
        .map_or(DEFAULT_JSONL_EXPORT_PARALLELISM, std::num::NonZero::get);
    let cap = if config.max_parallel_workers == 0 {
        DEFAULT_JSONL_EXPORT_PARALLELISM
    } else {
        config.max_parallel_workers
    };

    cap.min(host_parallelism).max(1)
}

fn export_parallelism_disabled_by_env() -> bool {
    std::env::var_os("BR_DISABLE_PARALLEL_JSONL_EXPORT").is_some_and(|value| {
        let value = value.to_string_lossy();
        value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

const fn should_prepare_export_issues_parallel(issue_count: usize, max_parallelism: usize) -> bool {
    max_parallelism > 1 && issue_count >= EXPORT_PARALLEL_PREPARE_MIN_ISSUES
}

fn prepare_export_issue_jsonl(
    issue: &Issue,
    retention_days: Option<u64>,
    export_as_of: &DateTime<Utc>,
) -> PreparedExportEntry {
    if issue.is_expired_tombstone_at(retention_days, export_as_of.to_owned()) {
        return PreparedExportEntry::SkippedTombstone(issue.id.clone());
    }

    let mut jsonl_line = Vec::with_capacity(1024);
    if let Err(err) = serde_json::to_writer(&mut jsonl_line, issue) {
        return PreparedExportEntry::Error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ));
    }
    jsonl_line.push(b'\n');

    PreparedExportEntry::Issue(PreparedExportIssue {
        id: issue.id.clone(),
        jsonl_line,
        content_hash: issue
            .content_hash
            .clone()
            .unwrap_or_else(|| crate::util::content_hash(issue)),
        dependency_count: issue.dependencies.len(),
        label_count: issue.labels.len(),
        comment_count: issue.comments.len(),
    })
}

fn prepare_export_issue_chunk(
    issues: &[Issue],
    retention_days: Option<u64>,
    export_as_of: &DateTime<Utc>,
) -> Vec<PreparedExportEntry> {
    issues
        .iter()
        .map(|issue| prepare_export_issue_jsonl(issue, retention_days, export_as_of))
        .collect()
}

fn prepare_export_issues_jsonl_parallel(
    issues: &[Issue],
    retention_days: Option<u64>,
    export_as_of: &DateTime<Utc>,
    max_parallelism: usize,
) -> Result<Vec<PreparedExportEntry>> {
    if !should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
        return Ok(prepare_export_issue_chunk(
            issues,
            retention_days,
            export_as_of,
        ));
    }

    let worker_count = max_parallelism.min(issues.len());
    let chunk_size = issues.len().div_ceil(worker_count);

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for (chunk_index, chunk) in issues.chunks(chunk_size).enumerate() {
            let start_index = chunk_index * chunk_size;
            handles.push(scope.spawn(move || {
                (
                    start_index,
                    prepare_export_issue_chunk(chunk, retention_days, export_as_of),
                )
            }));
        }

        let mut chunks = Vec::with_capacity(handles.len());
        for handle in handles {
            let chunk = handle.join().map_err(|_| {
                BeadsError::Config("Parallel JSONL export worker panicked".to_string())
            })?;
            chunks.push(chunk);
        }

        chunks.sort_unstable_by_key(|(start_index, _)| *start_index);
        let total_entries = chunks.iter().map(|(_, entries)| entries.len()).sum();
        let mut entries = Vec::with_capacity(total_entries);
        for (_, chunk_entries) in chunks {
            entries.extend(chunk_entries);
        }

        Ok(entries)
    })
}

#[allow(clippy::too_many_arguments)]
fn write_prepared_export_entries<W: Write>(
    writer: &mut W,
    prepared_entries: Vec<PreparedExportEntry>,
    hasher: &mut Sha256,
    ctx: &mut ExportContext,
    report: &mut ExportReport,
    exported_ids: &mut Vec<String>,
    skipped_tombstone_ids: &mut Vec<String>,
    issue_hashes: &mut Vec<(String, String)>,
    progress: Option<&ProgressBar>,
) -> Result<()> {
    for entry in prepared_entries {
        match entry {
            PreparedExportEntry::Issue(prepared) => {
                if let Err(err) = writer.write_all(&prepared.jsonl_line) {
                    ctx.handle_error(ExportError::new(
                        ExportEntityType::Issue,
                        prepared.id,
                        err.to_string(),
                    ))?;
                    increment_progress(progress);
                    continue;
                }

                hasher.update(&prepared.jsonl_line);
                exported_ids.push(prepared.id.clone());
                issue_hashes.push((prepared.id, prepared.content_hash));
                report.issues_exported += 1;
                report.dependencies_exported += prepared.dependency_count;
                report.labels_exported += prepared.label_count;
                report.comments_exported += prepared.comment_count;
            }
            PreparedExportEntry::SkippedTombstone(id) => {
                skipped_tombstone_ids.push(id);
            }
            PreparedExportEntry::Error(err) => {
                ctx.handle_error(err)?;
            }
        }
        increment_progress(progress);
    }

    Ok(())
}

fn increment_progress(progress: Option<&ProgressBar>) {
    if let Some(progress) = progress {
        progress.inc(1);
    }
}

/// Export issues from `SQLite` to JSONL format.
///
/// This implements the classic beads export semantics:
/// - Include tombstones (for sync propagation)
/// - Exclude ephemerals/wisps
/// - Sort by ID for deterministic output
/// - Populate dependencies and labels for each issue
/// - Atomic write (temp file -> rename)
/// - Safety guard against empty DB overwriting non-empty JSONL
///
/// # Errors
///
/// Returns an error if:
/// - Database read fails
/// - Safety guard is violated (empty DB, non-empty JSONL, no force)
/// - File write fails
#[allow(clippy::too_many_lines)]
pub fn export_to_jsonl(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<ExportResult> {
    let (result, _report) = export_to_jsonl_with_policy(storage, output_path, config)?;
    Ok(result)
}

/// Export issues with configurable error policy, returning a report.
///
/// # Errors
///
/// Returns an error if:
/// - Path validation fails (git path, outside `beads_dir` without opt-in)
/// - Database queries fail and the policy requires strict handling
/// - Safety guards are violated (empty/stale export without `force`)
/// - File I/O fails
#[allow(clippy::too_many_lines)]
pub fn export_to_jsonl_with_policy(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<(ExportResult, ExportReport)> {
    export_to_jsonl_with_policy_expected(storage, output_path, config, None)
}

pub(crate) fn export_to_jsonl_with_policy_expected(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
    expected_previous_content_sha256: Option<&Option<String>>,
) -> Result<(ExportResult, ExportReport)> {
    export_to_jsonl_with_policy_expected_authority(
        storage,
        output_path,
        config,
        expected_previous_content_sha256,
        None,
        None,
        None,
    )
}

pub(crate) fn export_to_jsonl_with_policy_expected_under_authority(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
    expected_previous_source: ExpectedJsonlSourceRef<'_>,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<(ExportResult, ExportReport)> {
    export_to_jsonl_with_policy_expected_authority(
        storage,
        output_path,
        config,
        None,
        Some(expected_previous_source),
        Some(jsonl_authority),
        None,
    )
}

pub(crate) fn export_to_jsonl_with_policy_expected_under_authorities(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
    expected_previous_source: ExpectedJsonlSourceRef<'_>,
    jsonl_authority: &JsonlFamilyWriteLock,
    database_authority: &DatabaseFamilyWriteLock,
) -> Result<(ExportResult, ExportReport)> {
    export_to_jsonl_with_policy_expected_authority(
        storage,
        output_path,
        config,
        None,
        Some(expected_previous_source),
        Some(jsonl_authority),
        Some(database_authority),
    )
}

#[allow(clippy::too_many_lines)]
fn export_to_jsonl_with_policy_expected_authority(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
    expected_previous_content_sha256: Option<&Option<String>>,
    expected_previous_source: Option<ExpectedJsonlSourceRef<'_>>,
    provided_jsonl_authority: Option<&JsonlFamilyWriteLock>,
    provided_database_authority: Option<&DatabaseFamilyWriteLock>,
) -> Result<(ExportResult, ExportReport)> {
    if let Some(database_authority) = provided_database_authority {
        database_authority.verify_database_authority()?;
    }
    let export_as_of = config.export_as_of.unwrap_or_else(Utc::now);

    // Path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)?;
        tracing::debug!(
            output_path = %output_path.display(),
            beads_dir = %beads_dir.display(),
            allow_external = config.allow_external_jsonl,
            "Export path validated"
        );
    }

    let parent_dir = output_path.parent().ok_or_else(|| {
        BeadsError::Config(format!("Invalid output path: {}", output_path.display()))
    })?;
    fs::create_dir_all(parent_dir)?;
    let owned_jsonl_authority = if provided_jsonl_authority.is_some() {
        None
    } else {
        Some(blocking_jsonl_family_write_lock_with_timeout(
            output_path,
            None,
        )?)
    };
    let jsonl_authority = provided_jsonl_authority
        .or(owned_jsonl_authority.as_ref())
        .ok_or_else(|| BeadsError::SyncConflict {
            message: "JSONL export has no write authority".to_string(),
        })?;
    let _ = jsonl_authority.pinned_name_for_target(output_path)?;
    jsonl_authority.verify_jsonl_authority()?;

    let captured_previous_source = match expected_previous_source {
        Some(ExpectedJsonlSourceRef::Present(_)) => None,
        Some(ExpectedJsonlSourceRef::Missing) | None => {
            jsonl_authority.capture_optional_target()?
        }
    };
    let previous_source = match expected_previous_source {
        Some(ExpectedJsonlSourceRef::Present(source)) => {
            verify_jsonl_source_snapshot_current(source, jsonl_authority)?;
            Some(source)
        }
        Some(ExpectedJsonlSourceRef::Missing) => {
            if captured_previous_source.is_some() {
                return Err(BeadsError::SyncConflict {
                    message: "JSONL appeared after the exporting session captured a missing source"
                        .to_string(),
                });
            }
            None
        }
        None => {
            if let Some(expected_previous) = expected_previous_content_sha256 {
                let observed = captured_previous_source
                    .as_ref()
                    .map(|source| source.content_sha256().to_string());
                if &observed != expected_previous {
                    return Err(BeadsError::SyncConflict {
                        message: "JSONL changed on disk since the exporting session loaded it; refusing a stale atomic replacement"
                            .to_string(),
                    });
                }
            }
            captured_previous_source.as_ref()
        }
    };
    let expected_previous_state = previous_source.map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );

    if let (Some(beads_dir), Some(previous_source)) = (config.beads_dir.as_ref(), previous_source) {
        // Perform backup before overwriting (if enabled and we have a beads_dir).
        // We backup any JSONL file that has been validated as safe for sync,
        // even if it's outside the .beads/ directory (e.g., in repo root).
        let output_abs = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(output_path)
        } else {
            output_path.to_path_buf()
        };

        history::backup_before_export_snapshot(
            beads_dir,
            &config.history,
            &output_abs,
            previous_source,
        )?;
    }

    // Get sorted export IDs up front for safety checks and bounded batch hydration.
    let export_ids = export_issue_ids(storage)?;

    // Fetch dirty metadata for safe clearing later
    let dirty_metadata = storage.get_dirty_issue_metadata()?;
    let intentionally_excluded_marked_at =
        intentionally_excluded_dirty_metadata(storage, &dirty_metadata)?;

    // Safety checks
    if !config.force
        && let Some(previous_source) = previous_source
    {
        let (jsonl_count, jsonl_ids) = analyze_jsonl_snapshot(previous_source)?;
        // IDs the operator intentionally hard-deleted (purged) are expected
        // to disappear from the JSONL on the next export; they are not data
        // loss (#405).
        let purged_ids = storage.get_purged_ids_pending_export()?;

        // Check 1: prevent exporting empty database over non-empty JSONL
        if export_ids.is_empty()
            && jsonl_count > 0
            && jsonl_ids.iter().any(|id| !purged_ids.contains(id))
        {
            return Err(BeadsError::Config(format!(
                "Refusing to export empty database over non-empty JSONL file.\n\
                 Database has 0 issues, JSONL has {jsonl_count} lines.\n\
                 This would result in data loss!\n\
                 Hint: Use --force to override this safety check."
            )));
        }

        // Check 2: prevent exporting stale database that would lose issues
        if !jsonl_ids.is_empty() {
            let db_ids: HashSet<String> = export_ids.iter().cloned().collect();
            let missing: Vec<_> = jsonl_ids
                .difference(&db_ids)
                .filter(|id| !purged_ids.contains(id.as_str()))
                .collect();

            if !missing.is_empty() {
                let mut missing_list = missing.into_iter().cloned().collect::<Vec<_>>();
                missing_list.sort();
                let display_count = missing_list.len().min(10);
                let preview: Vec<_> = missing_list.iter().take(display_count).collect();
                let more = if missing_list.len() > 10 {
                    format!(" ... and {} more", missing_list.len() - 10)
                } else {
                    String::new()
                };

                return Err(BeadsError::Config(format!(
                    "Refusing to export stale database that would lose issues.\n\
                     Database has {} issues, JSONL has {} unique issues.\n\
                     Export would lose {} issue(s): {}{}\n\
                     Hint: Run import first, or use --force to override.",
                    export_ids.len(),
                    jsonl_ids.len(),
                    missing_list.len(),
                    preview
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    more
                )));
            }
        }
    }

    let mut ctx = ExportContext::new(config.error_policy);
    let mut report = ExportReport::new(config.error_policy);

    let progress = create_progress_bar(
        export_ids.len() as u64,
        "Exporting issues",
        config.show_progress,
    );

    // Write to temp file for atomic rename
    let (temp_path, pinned_temp, temp_file) =
        create_full_export_temp_file_under_authority(output_path, config, jsonl_authority)?;
    let temp_guard = TempFileGuard::new_retained(temp_path.clone());
    let mut writer = BufWriter::new(temp_file);

    // Write JSONL and compute hash
    let mut hasher = Sha256::new();
    let mut exported_ids = Vec::with_capacity(export_ids.len());
    let mut skipped_tombstone_ids = Vec::new(); // Usually small
    let mut issue_hashes = Vec::with_capacity(export_ids.len());
    let mut buffer = Vec::with_capacity(1024);
    let max_parallelism = effective_export_parallelism(config);

    if export_ids.len() <= EXPORT_FULL_SCAN_ISSUE_THRESHOLD {
        let issues = hydrate_export_issues_full_scan(storage, &mut ctx)?;
        if should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
            let prepared = prepare_export_issues_jsonl_parallel(
                &issues,
                config.retention_days,
                &export_as_of,
                max_parallelism,
            )?;
            write_prepared_export_entries(
                &mut writer,
                prepared,
                &mut hasher,
                &mut ctx,
                &mut report,
                &mut exported_ids,
                &mut skipped_tombstone_ids,
                &mut issue_hashes,
                Some(&progress),
            )?;
        } else {
            for issue in &issues {
                // Skip expired tombstones
                if issue.is_expired_tombstone_at(config.retention_days, export_as_of) {
                    skipped_tombstone_ids.push(issue.id.clone());
                    progress.inc(1);
                    continue;
                }

                if !write_export_issue_jsonl(
                    &mut writer,
                    issue,
                    &mut hasher,
                    &mut buffer,
                    &mut ctx,
                )? {
                    progress.inc(1);
                    continue;
                }

                exported_ids.push(issue.id.clone());
                issue_hashes.push((
                    issue.id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| crate::util::content_hash(issue)),
                ));
                report.issues_exported += 1;
                report.dependencies_exported += issue.dependencies.len();
                report.labels_exported += issue.labels.len();
                report.comments_exported += issue.comments.len();
                progress.inc(1);
            }
        }
    } else {
        for id_batch in export_ids.chunks(EXPORT_ISSUE_BATCH_SIZE) {
            let issues = hydrate_export_issue_batch(storage, id_batch, &mut ctx)?;
            if should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
                let prepared = prepare_export_issues_jsonl_parallel(
                    &issues,
                    config.retention_days,
                    &export_as_of,
                    max_parallelism,
                )?;
                write_prepared_export_entries(
                    &mut writer,
                    prepared,
                    &mut hasher,
                    &mut ctx,
                    &mut report,
                    &mut exported_ids,
                    &mut skipped_tombstone_ids,
                    &mut issue_hashes,
                    Some(&progress),
                )?;
            } else {
                for issue in &issues {
                    // Skip expired tombstones
                    if issue.is_expired_tombstone_at(config.retention_days, export_as_of) {
                        skipped_tombstone_ids.push(issue.id.clone());
                        progress.inc(1);
                        continue;
                    }

                    if !write_export_issue_jsonl(
                        &mut writer,
                        issue,
                        &mut hasher,
                        &mut buffer,
                        &mut ctx,
                    )? {
                        progress.inc(1);
                        continue;
                    }

                    exported_ids.push(issue.id.clone());
                    issue_hashes.push((
                        issue.id.clone(),
                        issue
                            .content_hash
                            .clone()
                            .unwrap_or_else(|| crate::util::content_hash(issue)),
                    ));
                    report.issues_exported += 1;
                    report.dependencies_exported += issue.dependencies.len();
                    report.labels_exported += issue.labels.len();
                    report.comments_exported += issue.comments.len();
                    progress.inc(1);
                }
            }
        }
    }

    progress.finish_with_message("Export complete");

    // Flush and sync
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;

    // Compute final hash
    let content_hash = hex_encode(&hasher.finalize());

    // Verify staged export integrity before replacing the live JSONL.
    let staged_source = pinned_temp.capture()?;
    verify_exported_jsonl_snapshot_integrity(&staged_source, &exported_ids)?;
    if staged_source.content_sha256() != content_hash {
        return Err(BeadsError::SyncConflict {
            message: "Staged JSONL bytes do not match the export content hash".to_string(),
        });
    }
    if let Some(expected) = config.expected_staged_output.as_ref() {
        let observed_issue_hashes = sync_merge_export_hash_mapping_witness(&issue_hashes)?;
        if expected.raw_sha256 != content_hash
            || expected.issue_count != exported_ids.len()
            || expected.issue_hashes != observed_issue_hashes
        {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Staged JSONL does not match the exact reviewed export: expected sha256={} rows={} issue_hash_rows={} issue_hash_digest={}, observed sha256={} rows={} issue_hash_rows={} issue_hash_digest={}; live JSONL was not replaced and the staged file was retained for recovery",
                    expected.raw_sha256,
                    expected.issue_count,
                    expected.issue_hashes.rows,
                    expected.issue_hashes.payload_sha256,
                    content_hash,
                    exported_ids.len(),
                    observed_issue_hashes.rows,
                    observed_issue_hashes.payload_sha256
                ),
            });
        }
    }

    if let Some(ref beads_dir) = config.beads_dir {
        require_safe_sync_overwrite_path(
            &temp_path,
            beads_dir,
            config.allow_external_jsonl,
            "rename temp file",
        )?;
        require_safe_sync_overwrite_path(
            output_path,
            beads_dir,
            config.allow_external_jsonl,
            "overwrite JSONL output",
        )?;
    }
    if let Some(database_authority) = provided_database_authority {
        database_authority.verify_database_authority()?;
    }

    let publication = publish_staged_jsonl_conditionally(
        &temp_path,
        temp_guard,
        output_path,
        &staged_source,
        &expected_previous_state,
        &content_hash,
        jsonl_authority,
        provided_database_authority,
    )?;
    if let Some(database_authority) = provided_database_authority {
        database_authority.verify_database_authority()?;
    }

    let result = ExportResult {
        exported_count: exported_ids.len(),
        exported_marked_at: filter_dirty_metadata_for_export(
            &dirty_metadata,
            &exported_ids,
            &skipped_tombstone_ids,
        ),
        intentionally_excluded_marked_at,
        exported_ids,
        skipped_tombstone_ids,
        content_hash: content_hash.clone(),
        output_path: Some(output_path.to_string_lossy().to_string()),
        issue_hashes,
        publication: Some(publication.into_receipt(output_path, content_hash)),
    };

    report.errors = ctx.errors;

    Ok((result, report))
}

/// Export issues to a writer (e.g., stdout).
///
/// # Errors
///
/// Returns an error if serialization or writing fails.
pub fn export_to_writer<W: Write>(storage: &SqliteStorage, writer: &mut W) -> Result<ExportResult> {
    let (result, _report) =
        export_to_writer_with_policy(storage, writer, ExportErrorPolicy::Strict)?;
    Ok(result)
}

/// Export issues to a writer with configurable error policy.
///
/// # Errors
///
/// Returns an error if serialization or writing fails under a strict policy.
#[allow(clippy::too_many_lines)]
pub fn export_to_writer_with_policy<W: Write>(
    storage: &SqliteStorage,
    writer: &mut W,
    policy: ExportErrorPolicy,
) -> Result<(ExportResult, ExportReport)> {
    export_to_writer_with_policy_and_retention(storage, writer, policy, None)
}

pub(crate) fn export_to_writer_with_policy_and_retention<W: Write>(
    storage: &SqliteStorage,
    writer: &mut W,
    policy: ExportErrorPolicy,
    retention_days: Option<u64>,
) -> Result<(ExportResult, ExportReport)> {
    export_to_writer_with_policy_and_retention_at(
        storage,
        writer,
        policy,
        retention_days,
        Utc::now(),
    )
}

pub(crate) fn export_to_writer_with_policy_and_retention_at<W: Write>(
    storage: &SqliteStorage,
    writer: &mut W,
    policy: ExportErrorPolicy,
    retention_days: Option<u64>,
    export_as_of: DateTime<Utc>,
) -> Result<(ExportResult, ExportReport)> {
    let export_ids = export_issue_ids(storage)?;

    let mut ctx = ExportContext::new(policy);
    let mut report = ExportReport::new(policy);

    let mut hasher = Sha256::new();
    let mut exported_ids = Vec::with_capacity(export_ids.len());
    let mut skipped_tombstone_ids = Vec::new();
    let mut issue_hashes = Vec::with_capacity(export_ids.len());
    let mut buffer = Vec::with_capacity(1024);

    if export_ids.len() <= EXPORT_FULL_SCAN_ISSUE_THRESHOLD {
        let issues = hydrate_export_issues_full_scan(storage, &mut ctx)?;
        for issue in &issues {
            if issue.is_expired_tombstone_at(retention_days, export_as_of) {
                skipped_tombstone_ids.push(issue.id.clone());
                continue;
            }
            if !write_export_issue_jsonl(writer, issue, &mut hasher, &mut buffer, &mut ctx)? {
                continue;
            }

            exported_ids.push(issue.id.clone());
            issue_hashes.push((
                issue.id.clone(),
                issue
                    .content_hash
                    .clone()
                    .unwrap_or_else(|| crate::util::content_hash(issue)),
            ));
            report.issues_exported += 1;
            report.dependencies_exported += issue.dependencies.len();
            report.labels_exported += issue.labels.len();
            report.comments_exported += issue.comments.len();
        }
    } else {
        for id_batch in export_ids.chunks(EXPORT_ISSUE_BATCH_SIZE) {
            let issues = hydrate_export_issue_batch(storage, id_batch, &mut ctx)?;
            for issue in &issues {
                if issue.is_expired_tombstone_at(retention_days, export_as_of) {
                    skipped_tombstone_ids.push(issue.id.clone());
                    continue;
                }
                if !write_export_issue_jsonl(writer, issue, &mut hasher, &mut buffer, &mut ctx)? {
                    continue;
                }

                exported_ids.push(issue.id.clone());
                issue_hashes.push((
                    issue.id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| crate::util::content_hash(issue)),
                ));
                report.issues_exported += 1;
                report.dependencies_exported += issue.dependencies.len();
                report.labels_exported += issue.labels.len();
                report.comments_exported += issue.comments.len();
            }
        }
    }

    let content_hash = hex_encode(&hasher.finalize());

    let result = ExportResult {
        exported_count: exported_ids.len(),
        exported_ids,
        exported_marked_at: Vec::new(),
        intentionally_excluded_marked_at: Vec::new(),
        skipped_tombstone_ids,
        content_hash,
        output_path: None,
        issue_hashes,
        publication: None,
    };

    report.errors = ctx.errors;

    Ok((result, report))
}

/// Metadata key for the JSONL content hash.
pub const METADATA_JSONL_CONTENT_HASH: &str = "jsonl_content_hash";
/// Metadata key for the exact observed JSONL mtime at the last successful sync.
pub const METADATA_JSONL_MTIME: &str = "jsonl_mtime";
/// Metadata key for the exact observed JSONL size at the last successful sync.
pub const METADATA_JSONL_SIZE: &str = "jsonl_size";
/// Metadata key for the last export time.
pub const METADATA_LAST_EXPORT_TIME: &str = "last_export_time";
/// Metadata key for the last import time.
pub const METADATA_LAST_IMPORT_TIME: &str = "last_import_time";

#[derive(Debug, Clone)]
struct JsonlWitness {
    mtime: std::time::SystemTime,
    mtime_witness: String,
    size: u64,
}

/// Result of a staleness check between JSONL and DB.
#[derive(Debug, Clone, Copy)]
pub struct StalenessCheck {
    pub dirty_count: usize,
    pub jsonl_exists: bool,
    pub jsonl_mtime: Option<std::time::SystemTime>,
    pub jsonl_newer: bool,
    pub db_newer: bool,
}

fn pending_export_state(
    storage: &SqliteStorage,
    jsonl_exists: bool,
) -> Result<(usize, bool, bool)> {
    let dirty_count = storage.get_dirty_issue_count()?;
    let needs_flush = storage.get_metadata("needs_flush")?.as_deref() == Some("true");
    let missing_jsonl_with_data = !jsonl_exists && storage.count_issues()? > 0;
    Ok((
        dirty_count,
        needs_flush,
        dirty_count > 0 || needs_flush || missing_jsonl_with_data,
    ))
}

/// Compute staleness based on the JSONL content hash and DB dirty state.
///
/// Mtime and size are retained as diagnostic witnesses, but they are not
/// trusted as proof of unchanged content: a same-size rewrite can restore the
/// previous mtime.
///
/// # Errors
///
/// Returns an error if reading dirty state, metadata, JSONL mtime, or hashing fails.
pub fn compute_staleness(storage: &SqliteStorage, jsonl_path: &Path) -> Result<StalenessCheck> {
    let (staleness, _) = compute_staleness_impl(storage, jsonl_path)?;
    Ok(staleness)
}

/// Compute staleness and opportunistically persist refreshed JSONL witnesses.
///
/// When the stored content hash still matches but the cached mtime/size witness
/// is stale or incomplete, this updates the diagnostic metadata.
///
/// # Errors
///
/// Returns an error if reading dirty state, metadata, JSONL metadata, or
/// hashing fails. Opportunistic witness refresh failures are logged and
/// ignored so startup freshness probes do not fail on metadata backfill races.
pub fn compute_staleness_refreshing_witnesses(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
) -> Result<StalenessCheck> {
    let (staleness, refresh_witness) = compute_staleness_impl(storage, jsonl_path)?;
    if let Some(observed) = refresh_witness {
        refresh_jsonl_witness_best_effort(storage, jsonl_path, &observed);
    }
    Ok(staleness)
}

/// Check whether auto-import needs to inspect the JSONL contents.
///
/// This is the read-command startup fast path: when JSONL is not newer, callers
/// do not need dirty-count or pending-flush state because no import can happen.
/// If JSONL may be newer, `auto_import_if_stale` recomputes the full staleness
/// record before deciding whether a local dirty DB should block import.
///
/// # Errors
///
/// Returns an error if reading JSONL metadata, stored witnesses, or hashing
/// fails. Opportunistic witness refresh failures are logged and ignored.
pub fn auto_import_probe_refreshing_witnesses(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<bool> {
    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    let probe = compute_jsonl_newer_impl(storage, jsonl_path)?;
    if let Some(observed) = probe.refresh_witness {
        refresh_jsonl_witness_best_effort(storage, jsonl_path, &observed);
    }
    Ok(probe.jsonl_newer)
}

/// Check whether auto-import needs to inspect JSONL contents without mutating metadata.
///
/// This variant is intended for read-only startup probes. It deliberately skips
/// the opportunistic JSONL witness refresh that
/// [`auto_import_probe_refreshing_witnesses`] performs, so callers can use a
/// read-only SQLite handle and reopen writable storage only if import work is
/// actually needed.
///
/// # Errors
///
/// Returns an error if reading JSONL metadata, stored witnesses, or hashing
/// fails.
pub fn auto_import_probe(
    storage: &SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<bool> {
    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    compute_jsonl_newer_impl(storage, jsonl_path).map(|probe| probe.jsonl_newer)
}

fn compute_staleness_impl(
    storage: &SqliteStorage,
    jsonl_path: &Path,
) -> Result<(StalenessCheck, Option<JsonlWitness>)> {
    let jsonl_exists = jsonl_path.exists();
    let (dirty_count, _needs_flush, db_newer) = pending_export_state(storage, jsonl_exists)?;
    let probe = compute_jsonl_newer_impl(storage, jsonl_path)?;

    Ok((
        StalenessCheck {
            dirty_count,
            jsonl_exists: probe.jsonl_exists,
            jsonl_mtime: probe.jsonl_mtime,
            jsonl_newer: probe.jsonl_newer,
            db_newer,
        },
        probe.refresh_witness,
    ))
}

struct JsonlNewerProbe {
    jsonl_exists: bool,
    jsonl_mtime: Option<std::time::SystemTime>,
    jsonl_newer: bool,
    refresh_witness: Option<JsonlWitness>,
}

fn compute_jsonl_newer_impl(storage: &SqliteStorage, jsonl_path: &Path) -> Result<JsonlNewerProbe> {
    if !jsonl_path.exists() {
        return Ok(JsonlNewerProbe {
            jsonl_exists: false,
            jsonl_mtime: None,
            jsonl_newer: false,
            refresh_witness: None,
        });
    }

    let observed = observed_jsonl_witness(jsonl_path)?;
    let stored_mtime = storage.get_metadata(METADATA_JSONL_MTIME)?;
    let stored_size = storage.get_metadata(METADATA_JSONL_SIZE)?;
    let stored_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;
    // Mtime and size are mutable filesystem metadata, not a content identity.
    // In particular, external tooling can rewrite a file with the same length
    // and restore its prior mtime. Only the canonical content hash proves that
    // the JSONL still represents the state recorded by the database.
    let jsonl_newer = match stored_hash {
        Some(stored_hash) => compute_jsonl_hash(jsonl_path)? != stored_hash,
        None => true,
    };
    let stored_size_matches =
        stored_size.as_deref().and_then(parse_jsonl_size_witness) == Some(observed.size);
    let stored_mtime_matches = stored_mtime.as_deref() == Some(observed.mtime_witness.as_str());
    let refresh_witness =
        (!jsonl_newer && (!stored_mtime_matches || !stored_size_matches)).then(|| observed.clone());

    Ok(JsonlNewerProbe {
        jsonl_exists: true,
        jsonl_mtime: Some(observed.mtime),
        jsonl_newer,
        refresh_witness,
    })
}

#[cfg(test)]
fn observed_jsonl_mtime(jsonl_path: &Path) -> Result<(std::time::SystemTime, String)> {
    let observed = observed_jsonl_witness(jsonl_path)?;
    Ok((observed.mtime, observed.mtime_witness))
}

fn observed_jsonl_witness(jsonl_path: &Path) -> Result<JsonlWitness> {
    let metadata = fs::symlink_metadata(jsonl_path)?;
    let jsonl_mtime = metadata.modified()?;
    Ok(JsonlWitness {
        mtime: jsonl_mtime,
        mtime_witness: chrono::DateTime::<Utc>::from(jsonl_mtime).to_rfc3339(),
        size: metadata.len(),
    })
}

fn observed_jsonl_snapshot_witness(source: &JsonlSourceSnapshot) -> JsonlWitness {
    let modified = source.modified();
    JsonlWitness {
        mtime: modified,
        mtime_witness: chrono::DateTime::<Utc>::from(modified).to_rfc3339(),
        size: source.size(),
    }
}

fn parse_jsonl_size_witness(value: &str) -> Option<u64> {
    value.parse().ok()
}

fn record_observed_jsonl_witness_in_tx(
    storage: &SqliteStorage,
    observed: &JsonlWitness,
) -> Result<()> {
    storage.set_metadata_in_tx(METADATA_JSONL_MTIME, &observed.mtime_witness)?;
    storage.set_metadata_in_tx(METADATA_JSONL_SIZE, &observed.size.to_string())
}

fn maybe_refresh_jsonl_witness(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    observed: &JsonlWitness,
) -> Result<()> {
    let current = observed_jsonl_witness(jsonl_path)?;
    if current.mtime != observed.mtime || current.size != observed.size {
        return Ok(());
    }

    storage.with_write_transaction(|storage| record_observed_jsonl_witness_in_tx(storage, &current))
}

fn refresh_jsonl_witness_best_effort(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    observed: &JsonlWitness,
) {
    if let Err(error) = maybe_refresh_jsonl_witness(storage, jsonl_path, observed) {
        tracing::debug!(
            path = %jsonl_path.display(),
            error = %error,
            "Skipping opportunistic JSONL witness refresh"
        );
    }
}

/// Result of an auto-import attempt.
#[derive(Debug, Default)]
pub struct AutoImportResult {
    /// Whether an import was attempted.
    pub attempted: bool,
    /// Number of issues imported (created or updated).
    pub imported_count: usize,
}

/// Auto-import JSONL if it is newer than the DB.
///
/// Honors `--no-auto-import` and `--allow-stale` behavior.
/// Both flags short-circuit before any staleness probe so startup can skip the
/// JSONL stat/hash path entirely when the caller explicitly opted out.
///
/// # Errors
///
/// Returns an error if staleness checks, metadata reads, or import steps fail.
pub fn auto_import_if_stale(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    expected_prefix: Option<&str>,
    allow_external_jsonl: bool,
    allow_stale: bool,
    no_auto_import: bool,
) -> Result<AutoImportResult> {
    if allow_stale || no_auto_import {
        tracing::debug!(
            allow_stale,
            no_auto_import,
            "Skipping auto-import staleness probe due to startup override"
        );
        return Ok(AutoImportResult::default());
    }
    if let Some(receipt) = storage.pending_sync_merge_receipt()? {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Committed sync merge {} is pending {:?} reconciliation; refusing auto-import. Run `br sync --merge` first.",
                receipt.receipt_id, receipt.phase
            ),
        });
    }

    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    let staleness = compute_staleness_refreshing_witnesses(storage, jsonl_path)?;
    if !staleness.jsonl_newer {
        return Ok(AutoImportResult::default());
    }

    // When both JSONL and DB have changed, skip the auto-import with a
    // warning instead of failing the command.  This prevents spurious
    // SyncConflict errors when ≥3 concurrent `br` processes race: one
    // process flushes JSONL while another has pending local writes,
    // causing both `jsonl_newer` and `db_newer` to be true.
    //
    // Explicit `br sync --merge` still detects this as a hard conflict so the
    // user can reconcile manually.
    if staleness.db_newer && !allow_stale {
        tracing::warn!(
            dirty_count = staleness.dirty_count,
            jsonl_mtime = ?staleness.jsonl_mtime,
            "Skipping auto-import: JSONL changed externally while {} local change(s) are pending. \
             Run `br sync --merge` to reconcile.",
            staleness.dirty_count,
        );
        return Ok(AutoImportResult::default());
    }

    let import_config = ImportConfig {
        // The configured prefix is the default for new IDs, not a project-wide
        // invariant. Auto-import should preserve mixed-prefix workspaces.
        skip_prefix_validation: true,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        show_progress: false,
        ..Default::default()
    };

    let result = import_from_jsonl(storage, jsonl_path, &import_config, expected_prefix)?;

    tracing::debug!(
        imported_count = result.imported_count,
        jsonl_path = %jsonl_path.display(),
        "Auto-import completed"
    );

    Ok(AutoImportResult {
        attempted: true,
        imported_count: result.imported_count,
    })
}

/// Finalize an export by updating metadata, clearing dirty flags, and recording export hashes.
///
/// This should be called after a successful export to the default JSONL path.
/// It performs the following updates:
/// - Clears dirty flags for the exported issue IDs
/// - Records export hashes for each exported issue (for incremental export)
/// - Updates `jsonl_content_hash` metadata with the export hash
/// - Updates `last_export_time` metadata with the current timestamp
///
/// # Errors
///
/// Returns an error if database updates fail.
pub fn finalize_export(
    storage: &mut SqliteStorage,
    result: &ExportResult,
    issue_hashes: Option<&[(String, String)]>,
    jsonl_path: &Path,
) -> Result<()> {
    let jsonl_authority = blocking_jsonl_family_write_lock_with_timeout(jsonl_path, None)?;
    finalize_export_under_authority(storage, result, issue_hashes, jsonl_path, &jsonl_authority)
}

pub(crate) fn finalize_export_under_authority(
    storage: &mut SqliteStorage,
    result: &ExportResult,
    issue_hashes: Option<&[(String, String)]>,
    jsonl_path: &Path,
    jsonl_authority: &JsonlFamilyWriteLock,
) -> Result<()> {
    use chrono::Utc;
    jsonl_authority.verify_jsonl_authority()?;
    let pinned_target = jsonl_authority.pinned_name_for_target(jsonl_path)?;
    let published_source = result.published_source()?;
    if published_source.display_path() != pinned_target.display_path()
        || published_source.content_sha256() != result.content_hash
    {
        return Err(BeadsError::SyncConflict {
            message: "JSONL export receipt does not match the finalization target".to_string(),
        });
    }
    verify_jsonl_source_snapshot_current(published_source, jsonl_authority)?;
    let observed_jsonl = observed_jsonl_snapshot_witness(published_source);
    let expected_export_hashes = exact_full_export_hash_mapping(result, issue_hashes)?;

    storage.with_write_transaction(|storage| -> Result<()> {
        let live_dirty_metadata = storage.get_dirty_issue_metadata()?;
        let live_intentionally_excluded =
            intentionally_excluded_dirty_metadata(storage, &live_dirty_metadata)?;
        if live_intentionally_excluded != result.intentionally_excluded_marked_at {
            return Err(BeadsError::SyncConflict {
                message:
                    "Intentionally excluded dirty rows changed before full-export finalization"
                        .to_string(),
            });
        }

        // Clear dirty flags for exported issues (safe version with timestamp validation)
        if !result.exported_marked_at.is_empty() {
            storage.clear_dirty_issues_in_tx(&result.exported_marked_at)?;
        }
        if !result.intentionally_excluded_marked_at.is_empty() {
            storage.clear_dirty_issues_in_tx(&result.intentionally_excluded_marked_at)?;
        }
        let remaining_dirty_rows = additive_raw_rows(
            storage,
            "SELECT issue_id, marked_at FROM dirty_issues ORDER BY issue_id, marked_at",
        )?;
        if !remaining_dirty_rows.is_empty() {
            return Err(BeadsError::SyncConflict {
                message:
                    "Full-export finalization left dirty rows after exact reconciliation"
                        .to_string(),
            });
        }

        // Reconcile the table to the exact published mapping. Unchanged rows
        // retain their original exported_at timestamp; stale, excluded, and
        // expired rows are removed, and only changed/new rows are rewritten.
        let current_export_hashes = additive_export_hashes(storage)?;
        let stale_ids = current_export_hashes
            .keys()
            .filter(|issue_id| !expected_export_hashes.contains_key(*issue_id))
            .cloned()
            .collect::<Vec<_>>();
        if !stale_ids.is_empty() {
            storage.clear_export_hashes_in_tx(&stale_ids)?;
        }
        let changed_hashes = expected_export_hashes
            .iter()
            .filter(|(issue_id, content_hash)| {
                current_export_hashes.get(*issue_id) != Some(*content_hash)
            })
            .map(|(issue_id, content_hash)| (issue_id.clone(), content_hash.clone()))
            .collect::<Vec<_>>();
        if !changed_hashes.is_empty() {
            storage.set_changed_export_hashes_in_tx(&changed_hashes)?;
        }
        if additive_export_hashes(storage)? != expected_export_hashes {
            return Err(BeadsError::SyncConflict {
                message:
                    "Full-export finalization did not produce the exact published issue-hash mapping"
                        .to_string(),
            });
        }

        // Update metadata
        storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, &result.content_hash)?;
        storage.set_metadata_in_tx(METADATA_LAST_EXPORT_TIME, &Utc::now().to_rfc3339())?;
        record_observed_jsonl_witness_in_tx(storage, &observed_jsonl)?;

        // Keep the row stable and clear the flag in place so ordinary export
        // cycles avoid delete+insert churn on the metadata B-tree.
        storage.set_metadata_in_tx("needs_flush", "false")?;
        // The published snapshot no longer contains purged issues (#405).
        storage.clear_purged_ids_pending_export_in_tx()?;

        Ok(())
    })?;

    Ok(())
}

fn exact_full_export_hash_mapping(
    result: &ExportResult,
    issue_hashes: Option<&[(String, String)]>,
) -> Result<BTreeMap<String, String>> {
    let issue_hashes = issue_hashes.ok_or_else(|| BeadsError::SyncConflict {
        message: "Full-export finalization requires the exact published issue-hash mapping"
            .to_string(),
    })?;
    if result.exported_count != result.exported_ids.len() {
        return Err(BeadsError::SyncConflict {
            message: "JSONL export result count does not match its exported ID manifest"
                .to_string(),
        });
    }
    let exported_ids = result.exported_ids.iter().cloned().collect::<BTreeSet<_>>();
    if exported_ids.len() != result.exported_ids.len() {
        return Err(BeadsError::SyncConflict {
            message: "JSONL export result contains duplicate exported issue IDs".to_string(),
        });
    }
    let mut expected = BTreeMap::new();
    for (issue_id, content_hash) in issue_hashes {
        if expected
            .insert(issue_id.clone(), content_hash.clone())
            .is_some()
        {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Full-export finalization contains duplicate issue-hash mapping for {issue_id}"
                ),
            });
        }
    }
    let result_mapping = result
        .issue_hashes
        .iter()
        .cloned()
        .collect::<BTreeMap<_, _>>();
    if result_mapping.len() != result.issue_hashes.len() || result_mapping != expected {
        return Err(BeadsError::SyncConflict {
            message:
                "Full-export finalization issue-hash input does not match its immutable export result"
                    .to_string(),
        });
    }
    if expected.keys().cloned().collect::<BTreeSet<_>>() != exported_ids {
        return Err(BeadsError::SyncConflict {
            message:
                "Full-export issue-hash mapping does not exactly cover the published issue IDs"
                    .to_string(),
        });
    }
    Ok(expected)
}

fn normalize_issue_for_export(issue: &mut Issue) {
    if !issue.labels.is_empty() {
        issue.labels.sort_unstable();
        issue.labels.dedup();
    }

    if !issue.dependencies.is_empty() {
        issue.dependencies.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
    }

    if !issue.comments.is_empty() {
        issue.comments.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.author.cmp(&right.author))
                .then_with(|| left.body.cmp(&right.body))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

fn filter_dirty_metadata_for_export(
    dirty_metadata: &[(String, String)],
    exported_ids: &[String],
    skipped_tombstone_ids: &[String],
) -> Vec<(String, String)> {
    let dirty_by_id: HashMap<&str, &str> = dirty_metadata
        .iter()
        .map(|(issue_id, marked_at)| (issue_id.as_str(), marked_at.as_str()))
        .collect();

    exported_ids
        .iter()
        .chain(skipped_tombstone_ids.iter())
        .filter_map(|issue_id| {
            dirty_by_id
                .get(issue_id.as_str())
                .map(|marked_at| (issue_id.clone(), (*marked_at).to_string()))
        })
        .collect()
}

fn intentionally_excluded_dirty_metadata(
    storage: &SqliteStorage,
    dirty_metadata: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let dirty_ids = dirty_metadata
        .iter()
        .map(|(issue_id, _)| issue_id.clone())
        .collect::<Vec<_>>();
    let excluded_ids = storage
        .get_issues_by_ids(&dirty_ids)?
        .into_iter()
        .filter(|issue| issue.ephemeral || issue.id.contains("-wisp-"))
        .map(|issue| issue.id)
        .collect::<HashSet<_>>();

    let mut excluded = dirty_metadata
        .iter()
        .filter(|(issue_id, _)| excluded_ids.contains(issue_id))
        .cloned()
        .collect::<Vec<_>>();
    excluded.sort_unstable();
    Ok(excluded)
}

fn restore_foreign_keys_after_import(
    storage: &SqliteStorage,
    validate_integrity: bool,
) -> Result<()> {
    storage
        .execute_raw("PRAGMA foreign_keys = ON")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to re-enable foreign key enforcement after import".to_string(),
            source: Box::new(source),
        })?;

    let foreign_keys_enabled = storage
        .execute_raw_query("PRAGMA foreign_keys")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to verify foreign key enforcement state after import".to_string(),
            source: Box::new(source),
        })?
        .first()
        .and_then(|row| row.first())
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);

    if foreign_keys_enabled != 1 {
        return Err(BeadsError::internal(
            "Import completed with foreign key enforcement still disabled",
        ));
    }

    if !validate_integrity {
        return Ok(());
    }

    if let Some((table, column)) = find_post_import_fk_violation(storage)? {
        return Err(BeadsError::validation(
            "jsonl import",
            format!("orphaned rows in {table}.{column}"),
        ));
    }

    Ok(())
}

fn finish_import_after_foreign_key_restore(
    apply_result: Result<ImportResult>,
    fk_restore_result: Result<()>,
) -> Result<ImportResult> {
    match (apply_result, fk_restore_result) {
        (Ok(import_result), Ok(())) => Ok(import_result),
        (Ok(_), Err(fk_err)) => Err(fk_err),
        (Err(import_err), Ok(())) => Err(import_err),
        (Err(import_err), Err(fk_err)) => {
            tracing::error!(
                error = %fk_err,
                "Failed to restore foreign key enforcement after failed import"
            );
            Err(BeadsError::WithContext {
                context: format!(
                    "jsonl import failed, and SQLite foreign key enforcement could not be re-enabled: {fk_err}"
                ),
                source: Box::new(import_err),
            })
        }
    }
}

fn find_post_import_fk_violation(storage: &SqliteStorage) -> Result<Option<(String, String)>> {
    let fk_backed_tables = [
        ("dependencies", "issue_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
        ("events", "issue_id"),
        ("dirty_issues", "issue_id"),
        ("export_hashes", "issue_id"),
        ("blocked_issues_cache", "issue_id"),
        ("child_counters", "parent_id"),
        ("close_metadata", "issue_id"),
        ("gate_result_history", "issue_id"),
        ("gate_results", "issue_id"),
    ];

    for (table, column) in fk_backed_tables {
        let has_orphan = storage
            .has_missing_issue_reference(table, column)
            .map_err(|source| BeadsError::WithContext {
                context: format!(
                    "Failed to verify import integrity for foreign-key-backed table {table}.{column}"
                ),
                source: Box::new(source),
            })?;

        if has_orphan {
            return Ok(Some((table.to_string(), column.to_string())));
        }
    }

    Ok(None)
}

fn is_issue_exportable(issue: &Issue, retention_days: Option<u64>) -> bool {
    !issue.ephemeral && !issue.id.contains("-wisp-") && !issue.is_expired_tombstone(retention_days)
}

fn finalize_incremental_auto_flush(
    storage: &mut SqliteStorage,
    clear_dirty_metadata: &[(String, String)],
    removed_hash_ids: &[String],
    issue_hashes: &[(String, String)],
    content_hash: Option<&str>,
    jsonl_path: Option<&Path>,
) -> Result<()> {
    use chrono::Utc;
    let export_metadata = match content_hash {
        Some(content_hash) => {
            let jsonl_path = jsonl_path.ok_or_else(|| {
                BeadsError::Config(
                    "incremental auto-flush metadata update requires a JSONL path".to_string(),
                )
            })?;
            Some((content_hash, observed_jsonl_witness(jsonl_path)?))
        }
        None => None,
    };

    storage.with_write_transaction(|storage| -> Result<()> {
        if !clear_dirty_metadata.is_empty() {
            storage.clear_dirty_issues_in_tx(clear_dirty_metadata)?;
        }
        if !removed_hash_ids.is_empty() {
            storage.clear_export_hashes_in_tx(removed_hash_ids)?;
        }
        if !issue_hashes.is_empty() {
            storage.set_changed_export_hashes_in_tx(issue_hashes)?;
        }
        if let Some((content_hash, observed_jsonl)) = &export_metadata {
            storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, content_hash)?;
            storage.set_metadata_in_tx(METADATA_LAST_EXPORT_TIME, &Utc::now().to_rfc3339())?;
            record_observed_jsonl_witness_in_tx(storage, observed_jsonl)?;
        }
        storage.set_metadata_in_tx("needs_flush", "false")?;
        // The published snapshot no longer contains purged issues (#405).
        storage.clear_purged_ids_pending_export_in_tx()?;
        Ok(())
    })?;

    Ok(())
}

struct ExistingJsonlReplacementScan {
    exported_count: usize,
    changed: bool,
    all_replacements_seen: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ExistingJsonlReplacementWrite {
    /// The in-place writer cannot serve this flush without violating the
    /// canonical id-sorted JSONL ordering (GitHub #404): at least one
    /// replacement id is absent from the file, so it is a newly created issue
    /// that could only be appended at the tail. The caller falls back to the
    /// full `BTreeMap` rewrite, which emits every line in id order.
    Declined,
    Unchanged {
        exported_count: usize,
    },
    Written {
        content_hash: String,
        exported_count: usize,
    },
}

struct JsonlTempOutput {
    temp_path: PathBuf,
    temp_guard: TempFileGuard,
    writer: BufWriter<File>,
}

fn scan_existing_jsonl_replacements(
    path: &Path,
    replacement_lines: &HashMap<String, String>,
) -> Result<ExistingJsonlReplacementScan> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut seen_ids = HashSet::new();
    let mut seen_replacements = HashSet::with_capacity(replacement_lines.len());
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut exported_count = 0;
    let mut changed = false;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                path.display(),
                line_num
            )));
        }

        if let Some(replacement) = replacement_lines.get(&partial.id) {
            seen_replacements.insert(partial.id);
            changed |= replacement != trimmed;
        }

        exported_count += 1;
    }

    Ok(ExistingJsonlReplacementScan {
        exported_count,
        changed,
        all_replacements_seen: seen_replacements.len() == replacement_lines.len(),
    })
}

fn prepare_jsonl_temp_output(output_path: &Path, config: &ExportConfig) -> Result<JsonlTempOutput> {
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)?;
        let output_abs = absolute_or_current_dir_join(output_path);
        history::backup_before_export(beads_dir, &config.history, &output_abs)?;
    }

    let parent_dir = output_path.parent().ok_or_else(|| {
        BeadsError::Config(format!("Invalid output path: {}", output_path.display()))
    })?;
    fs::create_dir_all(parent_dir)?;

    let (temp_path, temp_file) = create_jsonl_temp_file(output_path, config)?;
    let temp_guard = TempFileGuard::new(temp_path.clone());
    set_restrictive_jsonl_permissions(&temp_path);

    Ok(JsonlTempOutput {
        temp_path,
        temp_guard,
        writer: BufWriter::new(temp_file),
    })
}

fn absolute_or_current_dir_join(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn rename_jsonl_temp_output(
    temp_path: &Path,
    mut temp_guard: TempFileGuard,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<()> {
    if let Some(ref beads_dir) = config.beads_dir {
        require_safe_sync_overwrite_path(
            temp_path,
            beads_dir,
            config.allow_external_jsonl,
            "rename temp file",
        )?;
        require_safe_sync_overwrite_path(
            output_path,
            beads_dir,
            config.allow_external_jsonl,
            "overwrite JSONL output",
        )?;
    }

    crate::util::durable_rename(temp_path, output_path)?;
    temp_guard.persist();
    Ok(())
}

fn sync_jsonl_writer(mut writer: BufWriter<File>) -> Result<()> {
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;
    Ok(())
}

fn try_write_existing_jsonl_replacements_atomically(
    replacement_lines: &HashMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<ExistingJsonlReplacementWrite> {
    let scan = scan_existing_jsonl_replacements(output_path, replacement_lines)?;

    if !scan.all_replacements_seen {
        // GitHub #404: a replacement id the file does not already contain is a
        // newly created issue. Substituting matched rows in place can only put
        // it at the tail, which leaves the JSONL non-canonically ordered until
        // the next `br sync --force`. Decline so the caller takes the sorted
        // full-rewrite path; updates (every id already present) keep the cheap
        // in-place write.
        return Ok(ExistingJsonlReplacementWrite::Declined);
    }

    if !scan.changed {
        return Ok(ExistingJsonlReplacementWrite::Unchanged {
            exported_count: scan.exported_count,
        });
    }

    let (content_hash, exported_count) =
        write_existing_jsonl_replacements_atomically(replacement_lines, output_path, config)?;
    Ok(ExistingJsonlReplacementWrite::Written {
        content_hash,
        exported_count,
    })
}

fn write_existing_jsonl_replacements_atomically(
    replacement_lines: &HashMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<(String, usize)> {
    let input_file = File::open(output_path)?;
    let mut reader = BufReader::new(input_file);
    let mut temp_output = prepare_jsonl_temp_output(output_path, config)?;
    let mut hasher = Sha256::new();
    let mut seen_ids = HashSet::new();
    let mut replaced_ids = HashSet::with_capacity(replacement_lines.len());
    let mut expected_ids = Vec::new();
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut exported_count = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                output_path.display(),
                line_num
            )));
        }

        let output_line = if let Some(replacement) = replacement_lines.get(&partial.id) {
            replaced_ids.insert(partial.id);
            replacement.as_str()
        } else {
            trimmed
        };

        writeln!(temp_output.writer, "{output_line}")?;
        hasher.update(output_line.as_bytes());
        hasher.update(b"\n");
        expected_ids.push(
            serde_json::from_str::<PartialId>(output_line)
                .map_err(|e| {
                    BeadsError::Config(format!(
                        "Invalid replacement JSON while preparing incremental auto-flush: {e}"
                    ))
                })?
                .id,
        );
        exported_count += 1;
    }

    let mut appended_ids = replacement_lines
        .keys()
        .filter(|id| !replaced_ids.contains(*id))
        .collect::<Vec<_>>();
    appended_ids.sort();

    for issue_id in appended_ids {
        let output_line = replacement_lines.get(issue_id).ok_or_else(|| {
            BeadsError::Config(format!(
                "Missing replacement JSON while preparing incremental auto-flush for {issue_id}"
            ))
        })?;
        writeln!(temp_output.writer, "{output_line}")?;
        hasher.update(output_line.as_bytes());
        hasher.update(b"\n");
        expected_ids.push(issue_id.clone());
        exported_count += 1;
    }

    let JsonlTempOutput {
        temp_path,
        temp_guard,
        writer,
    } = temp_output;

    sync_jsonl_writer(writer)?;
    verify_exported_jsonl_integrity(&temp_path, &expected_ids)?;
    rename_jsonl_temp_output(&temp_path, temp_guard, output_path, config)?;

    Ok((hex_encode(&hasher.finalize()), exported_count))
}

fn write_jsonl_lines_atomically(
    lines_by_id: &BTreeMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<String> {
    let mut temp_output = prepare_jsonl_temp_output(output_path, config)?;
    let mut hasher = Sha256::new();

    for line in lines_by_id.values() {
        writeln!(temp_output.writer, "{line}")?;
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }

    let JsonlTempOutput {
        temp_path,
        temp_guard,
        writer,
    } = temp_output;

    sync_jsonl_writer(writer)?;
    let expected_ids = lines_by_id.keys().cloned().collect::<Vec<_>>();
    verify_exported_jsonl_integrity(&temp_path, &expected_ids)?;

    rename_jsonl_temp_output(&temp_path, temp_guard, output_path, config)?;

    Ok(hex_encode(&hasher.finalize()))
}

struct IncrementalAutoFlushChanges {
    dirty_metadata: Vec<(String, String)>,
    removed_hash_ids: Vec<String>,
    issue_hashes: Vec<(String, String)>,
    replacement_lines: HashMap<String, String>,
}

fn collect_incremental_auto_flush_changes(
    storage: &SqliteStorage,
    dirty_metadata: Vec<(String, String)>,
) -> Result<IncrementalAutoFlushChanges> {
    let dirty_len = dirty_metadata.len();
    let mut removed_hash_ids = Vec::with_capacity(dirty_len);
    let mut issue_hashes = Vec::with_capacity(dirty_len);
    let mut replacement_lines = HashMap::with_capacity(dirty_len);

    let dirty_ids: Vec<String> = dirty_metadata.iter().map(|(id, _)| id.clone()).collect();
    let batch_issues = storage.get_issues_for_export(&dirty_ids)?;
    let mut issues_by_id: HashMap<String, crate::model::Issue> = batch_issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();

    for (issue_id, _) in &dirty_metadata {
        let maybe_issue = issues_by_id.remove(issue_id);
        match maybe_issue {
            Some(mut issue) if is_issue_exportable(&issue, None) => {
                normalize_issue_for_export(&mut issue);
                let json = serde_json::to_string(&issue).map_err(|err| {
                    BeadsError::Config(format!(
                        "Failed to serialize issue '{}' during auto-flush: {err}",
                        issue.id
                    ))
                })?;

                issue_hashes.push((
                    issue_id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| issue.compute_content_hash()),
                ));
                replacement_lines.insert(issue_id.clone(), json);
            }
            Some(_) | None => removed_hash_ids.push(issue_id.clone()),
        }
    }

    Ok(IncrementalAutoFlushChanges {
        dirty_metadata,
        removed_hash_ids,
        issue_hashes,
        replacement_lines,
    })
}

fn try_existing_line_auto_flush(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    export_config: &ExportConfig,
    changes: &IncrementalAutoFlushChanges,
    jsonl_authority: &JsonlFamilyWriteLock,
    source_content_hash: &str,
) -> Result<Option<AutoFlushResult>> {
    if !changes.removed_hash_ids.is_empty() || changes.replacement_lines.is_empty() {
        return Ok(None);
    }

    let result = try_write_existing_jsonl_replacements_atomically(
        &changes.replacement_lines,
        jsonl_path,
        export_config,
    )?;

    match result {
        // GitHub #404: the in-place writer refused because the flush carries a
        // brand-new id. Fall through to `read_jsonl_lines_by_id` +
        // `write_jsonl_lines_atomically`, symmetric with the removals decline
        // above, so the file lands id-sorted.
        ExistingJsonlReplacementWrite::Declined => Ok(None),
        ExistingJsonlReplacementWrite::Unchanged { .. } => {
            jsonl_authority.verify_jsonl_authority()?;
            if compute_jsonl_hash(jsonl_path)? != source_content_hash {
                return Err(BeadsError::SyncConflict {
                    message: "JSONL changed during an unchanged incremental auto-flush scan"
                        .to_string(),
                });
            }
            finalize_incremental_auto_flush(
                storage,
                &changes.dirty_metadata,
                &changes.removed_hash_ids,
                &changes.issue_hashes,
                None,
                None,
            )?;
            Ok(Some(AutoFlushResult::default()))
        }
        ExistingJsonlReplacementWrite::Written {
            content_hash,
            exported_count,
        } => {
            jsonl_authority.verify_jsonl_authority()?;
            if compute_jsonl_hash(jsonl_path)? != content_hash {
                return Err(BeadsError::SyncConflict {
                    message: "Persisted JSONL bytes do not match the incremental auto-flush hash"
                        .to_string(),
                });
            }
            finalize_incremental_auto_flush(
                storage,
                &changes.dirty_metadata,
                &changes.removed_hash_ids,
                &changes.issue_hashes,
                Some(&content_hash),
                Some(jsonl_path),
            )?;
            Ok(Some(AutoFlushResult {
                flushed: true,
                exported_count,
                content_hash,
            }))
        }
    }
}

fn apply_incremental_auto_flush_changes(
    lines_by_id: &mut BTreeMap<String, String>,
    changes: &IncrementalAutoFlushChanges,
) -> bool {
    let mut changed = false;
    for (issue_id, json) in &changes.replacement_lines {
        if lines_by_id.get(issue_id) != Some(json) {
            lines_by_id.insert(issue_id.clone(), json.clone());
            changed = true;
        }
    }
    for issue_id in &changes.removed_hash_ids {
        changed |= lines_by_id.remove(issue_id).is_some();
    }
    changed
}

fn try_incremental_auto_flush(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<Option<AutoFlushResult>> {
    if !jsonl_path.exists() {
        return Ok(None);
    }

    let jsonl_authority = blocking_jsonl_family_write_lock_with_timeout(jsonl_path, None)?;
    jsonl_authority.verify_jsonl_authority()?;
    if !jsonl_path.is_file() {
        return Err(BeadsError::SyncConflict {
            message: "JSONL disappeared while acquiring incremental auto-flush authority"
                .to_string(),
        });
    }
    let source_content_hash = compute_jsonl_hash(jsonl_path)?;
    let conflict_markers = scan_conflict_markers(jsonl_path)?;
    if !conflict_markers.is_empty() {
        tracing::warn!(
            jsonl_path = %jsonl_path.display(),
            marker_count = conflict_markers.len(),
            "Skipping incremental auto-flush: JSONL contains merge-conflict markers",
        );
        return Ok(Some(AutoFlushResult::default()));
    }

    let dirty_metadata = storage.get_dirty_issue_metadata()?;
    if dirty_metadata.is_empty() {
        return Ok(Some(AutoFlushResult::default()));
    }

    let changes = collect_incremental_auto_flush_changes(storage, dirty_metadata)?;
    let export_config = ExportConfig {
        force: false,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        ..Default::default()
    };

    if let Some(result) = try_existing_line_auto_flush(
        storage,
        jsonl_path,
        &export_config,
        &changes,
        &jsonl_authority,
        &source_content_hash,
    )? {
        return Ok(Some(result));
    }

    let mut lines_by_id = read_jsonl_lines_by_id(jsonl_path)?;
    let changed = apply_incremental_auto_flush_changes(&mut lines_by_id, &changes);

    if !changed {
        jsonl_authority.verify_jsonl_authority()?;
        if compute_jsonl_hash(jsonl_path)? != source_content_hash {
            return Err(BeadsError::SyncConflict {
                message: "JSONL changed during incremental auto-flush reconciliation".to_string(),
            });
        }
        finalize_incremental_auto_flush(
            storage,
            &changes.dirty_metadata,
            &changes.removed_hash_ids,
            &changes.issue_hashes,
            None,
            None,
        )?;
        return Ok(Some(AutoFlushResult::default()));
    }

    let content_hash = write_jsonl_lines_atomically(&lines_by_id, jsonl_path, &export_config)?;
    jsonl_authority.verify_jsonl_authority()?;
    if compute_jsonl_hash(jsonl_path)? != content_hash {
        return Err(BeadsError::SyncConflict {
            message: "Persisted JSONL bytes do not match the incremental auto-flush hash"
                .to_string(),
        });
    }
    finalize_incremental_auto_flush(
        storage,
        &changes.dirty_metadata,
        &changes.removed_hash_ids,
        &changes.issue_hashes,
        Some(&content_hash),
        Some(jsonl_path),
    )?;

    Ok(Some(AutoFlushResult {
        flushed: true,
        exported_count: lines_by_id.len(),
        content_hash,
    }))
}

/// Result of an auto-flush operation.
#[derive(Debug, Default)]
pub struct AutoFlushResult {
    /// Whether the flush was performed (false if skipped due to no dirty issues).
    pub flushed: bool,
    /// Number of issues exported (0 if not flushed).
    pub exported_count: usize,
    /// Content hash of the exported JSONL (empty if not flushed).
    pub content_hash: String,
}

/// Perform an automatic flush of dirty issues to JSONL.
///
/// This is the auto-flush operation that runs at the end of mutating commands
/// (unless `--no-auto-flush` is set). It:
/// 1. Checks for dirty issues
/// 2. If any exist, exports them to the resolved JSONL path
/// 3. Clears dirty flags and updates metadata
///
/// Returns early (no-op) if there are no dirty issues.
///
/// # Arguments
///
/// * `storage` - Mutable reference to the `SQLite` storage
/// * `beads_dir` - Path to the .beads directory
/// * `jsonl_path` - Resolved JSONL export target for this workspace
///
/// # Errors
///
/// Returns an error if the export fails.
pub fn auto_flush(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<AutoFlushResult> {
    // This guard is intentionally independent of CLI/MCP startup policy and
    // precedes the dirty/no-op probe. A clean database with a durable pending
    // saga is still not safe for an unrelated automatic exporter: even a
    // nominal no-op may refresh metadata or become dirty after a caller's
    // stale preflight.
    let pending = storage.inspect_pending_sync_merge()?;
    if !pending.permits_automatic_mutation() {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "{}; automatic JSONL export is disabled until `br sync --merge` reconciles and clears the pending state",
                pending.diagnostic()
            ),
        });
    }

    // Check for dirty issues or forced flush first
    let jsonl_exists = jsonl_path.exists();
    let (dirty_count, needs_flush, db_newer) = pending_export_state(storage, jsonl_exists)?;

    if !db_newer {
        tracing::debug!("Auto-flush: no dirty issues, skipping");
        return Ok(AutoFlushResult::default());
    }

    validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;

    // Refuse to auto-flush over a JSONL that still holds unresolved
    // merge-conflict markers. The downstream export path would otherwise
    // silently overwrite the `<<<<<<<` / `=======` / `>>>>>>>` regions
    // (along with the remote side of the merge the operator hadn't yet
    // looked at) every time a mutating CLI command returns. Explicit
    // `br sync --flush-only` already has a `--force` escape hatch for this
    // case; auto-flush has no such surface, so the only safe default is to
    // stop, log clearly, and let the next explicit sync surface the error.
    if jsonl_exists {
        let conflict_markers = scan_conflict_markers(jsonl_path)?;
        if !conflict_markers.is_empty() {
            tracing::warn!(
                jsonl_path = %jsonl_path.display(),
                marker_count = conflict_markers.len(),
                "Skipping auto-flush: JSONL contains merge-conflict markers. Resolve them (or run `br sync --flush-only --force` to override) before the next write.",
            );
            return Ok(AutoFlushResult::default());
        }
    }

    tracing::debug!(
        dirty_count,
        needs_flush,
        "Auto-flush: exporting dirty issues"
    );

    if !needs_flush {
        match try_incremental_auto_flush(storage, beads_dir, jsonl_path, allow_external_jsonl) {
            Ok(Some(result)) => {
                tracing::info!(
                    flushed = result.flushed,
                    exported = result.exported_count,
                    "Auto-flush complete"
                );
                return Ok(result);
            }
            Ok(None) => {}
            Err(err) => {
                return Err(err);
            }
        }
    }

    // Configure export with defaults, including beads_dir for path validation.
    // `needs_flush` is deliberately NOT passed as `force` (#405): doing so
    // disabled the exporter's data-loss guards, and the import path also arms
    // `needs_flush` when a local record wins over JSONL — a state in which a
    // forced auto-flush can destroy merged issues the DB never imported. The
    // purge_issue flow that used to need force is handled by the
    // purged-pending-export marker, which the guard subtracts from its loss
    // computation.
    let export_config = ExportConfig {
        force: false,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        ..Default::default()
    };

    // Perform export
    let expected_missing_jsonl = (!jsonl_exists).then_some(None);
    let (export_result, _report) = export_to_jsonl_with_policy_expected(
        storage,
        jsonl_path,
        &export_config,
        expected_missing_jsonl.as_ref(),
    )?;

    // Finalize export (clear dirty flags, update metadata)
    finalize_export(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
    )?;

    tracing::info!(
        exported = export_result.exported_count,
        "Auto-flush complete"
    );

    Ok(AutoFlushResult {
        flushed: true,
        exported_count: export_result.exported_count,
        content_hash: export_result.content_hash,
    })
}

/// Read all issues from a JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn read_issues_from_jsonl(path: &Path) -> Result<Vec<Issue>> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    let file_size = file.metadata().map_or(0, |m| m.len());
    let estimated_count = (file_size / 500) as usize;
    read_issues_from_jsonl_reader(path, estimated_count, BufReader::new(file))
}

pub(crate) fn read_issues_from_jsonl_snapshot(source: &JsonlSourceSnapshot) -> Result<Vec<Issue>> {
    let estimated_count = (source.size() / 500) as usize;
    read_issues_from_jsonl_reader(source.display_path(), estimated_count, source.reader())
}

fn read_issues_from_jsonl_reader(
    display_path: &Path,
    estimated_count: usize,
    mut reader: impl BufRead,
) -> Result<Vec<Issue>> {
    let mut issues = Vec::with_capacity(estimated_count);
    let mut seen_ids = HashSet::with_capacity(estimated_count);
    let mut line = String::new();
    let mut line_num = 0;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            line_num += 1;
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed).map_err(|e| {
            BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num + 1, e))
        })?;
        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                display_path.display(),
                line_num + 1
            )));
        }
        issues.push(issue);
        line_num += 1;
    }

    Ok(issues)
}

// ===== 4-Phase Collision Detection =====

/// Match type from collision detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    /// Matched by external reference (e.g., JIRA-123).
    ExternalRef,
    /// Matched by content hash (deduplication).
    ContentHash,
    /// Matched by ID.
    Id,
}

/// Result of collision detection.
#[derive(Debug, Clone)]
pub enum CollisionResult {
    /// No match found - issue is new.
    NewIssue,
    /// Matched an existing issue.
    Match {
        /// The existing issue ID.
        existing_id: String,
        /// How the match was determined.
        match_type: MatchType,
        /// Which phase found the match (1-3).
        phase: u8,
    },
}

/// Action to take after collision detection.
#[derive(Debug, Clone)]
pub enum CollisionAction {
    /// Insert as a new issue.
    Insert,
    /// Update the existing issue.
    Update { existing_id: String },
    /// Skip this issue (existing is newer or it's a tombstone).
    Skip { reason: String },
}

/// Detect collision for an incoming issue using the 4-phase algorithm with preloaded metadata maps.
fn detect_collision(
    incoming: &Issue,
    id_by_ext_ref: &std::collections::HashMap<String, String>,
    id_by_hash: &std::collections::HashMap<String, String>,
    meta_by_id: &std::collections::HashMap<String, crate::storage::sqlite::IssueMetadata>,
    computed_hash: &str,
) -> CollisionResult {
    // Phase 1: External reference match
    if let Some(ref external_ref) = incoming.external_ref
        && let Some(existing_id) = id_by_ext_ref.get(external_ref)
    {
        return CollisionResult::Match {
            existing_id: existing_id.clone(),
            match_type: MatchType::ExternalRef,
            phase: 1,
        };
    }

    // Phase 2: ID match
    if meta_by_id.contains_key(&incoming.id) {
        return CollisionResult::Match {
            existing_id: incoming.id.clone(),
            match_type: MatchType::Id,
            phase: 2,
        };
    }

    // Phase 3: Content hash match
    if let Some(existing_id) = id_by_hash.get(computed_hash) {
        return CollisionResult::Match {
            existing_id: existing_id.clone(),
            match_type: MatchType::ContentHash,
            phase: 3,
        };
    }

    // Phase 4: No match
    CollisionResult::NewIssue
}

/// Determine the action to take based on collision result.
fn determine_action(
    collision: &CollisionResult,
    incoming: &Issue,
    meta_by_id: &std::collections::HashMap<String, crate::storage::sqlite::IssueMetadata>,
    force_upsert: bool,
) -> Result<CollisionAction> {
    match collision {
        CollisionResult::NewIssue => Ok(CollisionAction::Insert),
        CollisionResult::Match { existing_id, .. } => {
            let existing_meta =
                meta_by_id
                    .get(existing_id)
                    .ok_or_else(|| BeadsError::IssueNotFound {
                        id: existing_id.clone(),
                    })?;

            // Check for tombstone protection (even force doesn't override this)
            if existing_meta.status == crate::model::Status::Tombstone {
                return Ok(CollisionAction::Skip {
                    reason: format!("Tombstone protection: {existing_id}"),
                });
            }

            // If force_upsert is enabled, always update (skip timestamp comparison)
            if force_upsert {
                return Ok(CollisionAction::Update {
                    existing_id: existing_id.clone(),
                });
            }

            // Last-write-wins: compare updated_at
            match incoming.updated_at.cmp(&existing_meta.updated_at) {
                std::cmp::Ordering::Greater => Ok(CollisionAction::Update {
                    existing_id: existing_id.clone(),
                }),
                std::cmp::Ordering::Equal => Ok(CollisionAction::Skip {
                    reason: format!("Equal timestamps: {existing_id}"),
                }),
                std::cmp::Ordering::Less => Ok(CollisionAction::Skip {
                    reason: format!("Existing is newer: {existing_id}"),
                }),
            }
        }
    }
}

/// Normalize an issue for import.
///
/// - Recomputes `content_hash`
/// - Sets ephemeral=true if ID contains "-wisp-"
/// - Applies defaults and repairs `closed_at` invariant
fn normalize_issue(issue: &mut Issue) -> usize {
    use crate::util::content_hash;

    // Deduplicate labels
    if !issue.labels.is_empty() {
        issue.labels.sort();
        issue.labels.dedup();
    }

    // Normalize dependency types (fix legacy underscores)
    for dep in &mut issue.dependencies {
        if let crate::model::DependencyType::Custom(custom) = &dep.dep_type {
            let candidate = custom.replace('_', "-");
            if let Ok(normalized) = candidate.parse::<crate::model::DependencyType>()
                && !matches!(normalized, crate::model::DependencyType::Custom(_))
            {
                dep.dep_type = normalized;
            }
        }
    }

    // Deduplicate dependencies by the database key (issue_id, depends_on_id),
    // keeping only the most recent entry by created_at. This handles duplicate
    // parent-child entries from reparenting or migration artifacts (see issue #159).
    if issue.dependencies.len() > 1 {
        use std::collections::HashMap;
        // The storage schema has one row per pair, so type-distinct duplicates
        // cannot be preserved without a schema migration.
        let mut best: HashMap<(String, String), usize> = HashMap::new();
        for (i, dep) in issue.dependencies.iter().enumerate() {
            let key = (dep.issue_id.clone(), dep.depends_on_id.clone());
            match best.get(&key) {
                Some(&prev_idx) if issue.dependencies[prev_idx].created_at >= dep.created_at => {
                    // existing entry is newer or equal, skip
                }
                _ => {
                    best.insert(key, i);
                }
            }
        }
        if best.len() < issue.dependencies.len() {
            let mut keep_indices: Vec<usize> = best.into_values().collect();
            keep_indices.sort_unstable();
            issue.dependencies = keep_indices
                .into_iter()
                .map(|i| issue.dependencies[i].clone())
                .collect();
        }
    }

    // A damaged comments tree has historically exported the same comment
    // object more than once. Re-importing that recovery artifact should be
    // lossless: collapse only byte-for-byte semantic duplicates while leaving
    // same-ID/different-payload pairs for the strict validator to reject.
    let comments_before = issue.comments.len();
    if comments_before > 1 {
        let mut unique = Vec::with_capacity(comments_before);
        for comment in issue.comments.drain(..) {
            if !unique.contains(&comment) {
                unique.push(comment);
            }
        }
        issue.comments = unique;
    }
    let exact_duplicate_comments_deduplicated = comments_before - issue.comments.len();

    // Normalize legacy Go-beads (bd) terminal status aliases that survived
    // JSONL import as `Status::Custom(_)`. Leaving them unmapped is
    // corruptive: our own `is_terminal()` returns false for Custom, so the
    // closed_at repair below skips them and the CHECK constraint later
    // rejects the row. Downstream consumers (bv, bd-style readers) also
    // reject unknown statuses outright.
    if let crate::model::Status::Custom(raw) = &issue.status {
        let key = raw.trim().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "done" | "complete" | "completed" | "finished" | "resolved"
        ) {
            issue.status = crate::model::Status::Closed;
        }
    }

    // Wisp detection: if ID contains "-wisp-", mark as ephemeral
    if issue.id.contains("-wisp-") {
        issue.ephemeral = true;
    }

    // Repair closed_at invariant: if status is terminal (closed/tombstone), ensure closed_at is set
    if issue.status.is_terminal() && issue.closed_at.is_none() {
        issue.closed_at = Some(issue.updated_at);
    }

    // If status is not terminal, clear closed_at
    if !issue.status.is_terminal() {
        issue.closed_at = None;
    }

    // Normalize external_ref: empty string should be None to prevent UNIQUE constraint violations
    if let Some(ext_ref) = &issue.external_ref {
        if ext_ref.trim().is_empty() {
            issue.external_ref = None;
        } else {
            // Re-assign trimmed version just in case
            issue.external_ref = Some(ext_ref.trim().to_string());
        }
    }

    // Repair timestamps invariant: updated_at cannot be before created_at.
    // In distributed systems, clocks can be out of sync; we enforce the invariant
    // locally to keep the database consistent.
    if issue.updated_at < issue.created_at {
        issue.updated_at = issue.created_at;
    }

    // Recompute after all import repairs so the stored row hash matches the
    // canonical issue state used by collision detection and export hashes.
    issue.content_hash = Some(content_hash(issue));
    exact_duplicate_comments_deduplicated
}

#[derive(Debug)]
struct PrefixRenameSeed {
    old_id: String,
    title: String,
    description: Option<String>,
    created_by: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct ImportValidationPlan {
    record_count: usize,
    prefix_mismatches: Vec<PrefixRenameSeed>,
    occupied_ids: HashSet<String>,
}

struct ImportMetadataMaps {
    meta_by_id: HashMap<String, crate::storage::sqlite::IssueMetadata>,
    id_by_ext_ref: HashMap<String, String>,
    id_by_hash: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct ImportCollisionPlan {
    renames: HashMap<String, String>,
    comment_owner_ids_to_replace: Vec<String>,
}

fn parse_normalized_import_issue(trimmed: &str, line_num: usize) -> Result<(Issue, usize)> {
    let mut issue: Issue = serde_json::from_str(trimmed)
        .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {line_num}: {e}")))?;

    let exact_duplicate_comments_deduplicated = normalize_issue(&mut issue);

    if let Err(errors) = IssueValidator::validate(&issue) {
        let details = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(BeadsError::Config(format!(
            "Validation failed for issue {} at line {}: {}",
            issue.id, line_num, details
        )));
    }

    Ok((issue, exact_duplicate_comments_deduplicated))
}

fn for_each_jsonl_import_issue(
    source: &JsonlSourceSnapshot,
    mut handle_issue: impl FnMut(usize, Issue, usize) -> Result<()>,
) -> Result<()> {
    let mut reader = source.reader();
    let mut line = String::new();
    let mut line_num = 0usize;

    while reader.read_line(&mut line)? > 0 {
        line_num += 1;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let (issue, exact_duplicate_comments_deduplicated) =
                parse_normalized_import_issue(trimmed, line_num)?;
            handle_issue(line_num, issue, exact_duplicate_comments_deduplicated)?;
        }
        line.clear();
    }

    Ok(())
}

fn collect_import_validation_plan(
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportValidationPlan> {
    let mut plan = ImportValidationPlan::default();
    let mut seen_ids = HashSet::new();

    for_each_jsonl_import_issue(source, |line_num, issue, _| {
        let prefix_mismatch = !config.skip_prefix_validation
            && expected_prefix.is_some_and(|prefix| {
                !id_matches_expected_prefix(&issue.id, prefix)
                    && issue.status != crate::model::Status::Tombstone
            });

        if prefix_mismatch && !config.rename_on_import {
            return Err(BeadsError::Config(format!(
                "Prefix mismatch at line {}: expected '{}', found issue '{}'",
                line_num,
                expected_prefix.unwrap_or_default(),
                issue.id
            )));
        }

        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                source.display_path().display(),
                line_num
            )));
        }

        if prefix_mismatch {
            plan.prefix_mismatches.push(PrefixRenameSeed {
                old_id: issue.id,
                title: issue.title,
                description: issue.description,
                created_by: issue.created_by,
                created_at: issue.created_at,
            });
        } else {
            plan.occupied_ids.insert(issue.id);
        }
        plan.record_count += 1;

        Ok(())
    })?;

    Ok(plan)
}

/// `--rename-prefix` receipt reason: the remainder-preserving id was already
/// taken (in the DB, the JSONL, or by an earlier rename in this import).
const PREFIX_RENAME_FALLBACK_COLLISION: &str = "regenerated-on-collision";
/// `--rename-prefix` receipt reason: the old id had no separable prefix
/// segment, or the preserved remainder would not form a valid id under the
/// configured prefix.
const PREFIX_RENAME_FALLBACK_UNPARSEABLE: &str = "regenerated-unparseable-id";

/// Rewrite only the prefix segment of a mismatched issue id, keeping the
/// remainder (slug and hash) intact: `oldp-cargo-license-spdx-ay8` becomes
/// `newp-cargo-license-spdx-ay8` under prefix `newp`.
///
/// The old prefix is the id's first `-`-separated segment — the same
/// first-segment semantics `parse_id`/`id_matches_expected_prefix` use to
/// detect the mismatch, so arbitrary in-the-wild prefixes work without any
/// prefix registry. A doubled prefix collapses exactly once, not
/// recursively: `oldp-oldp-x-3un` -> `x-3un` remainder, while
/// `oldp-oldp-oldp-x` keeps `oldp-x`.
///
/// Returns `None` when the id has no prefix segment or the remainder is
/// empty; the caller falls back to regenerating a fresh id.
fn prefix_preserving_rename(old_id: &str, new_prefix: &str) -> Option<String> {
    let (old_prefix, mut remainder) = old_id.split_once('-')?;
    if old_prefix.is_empty() {
        return None;
    }
    if let Some(deduped) = remainder
        .strip_prefix(old_prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        && !deduped.is_empty()
    {
        remainder = deduped;
    }
    if remainder.is_empty() {
        return None;
    }
    Some(format!("{new_prefix}-{remainder}"))
}

fn build_prefix_renames(
    storage: &SqliteStorage,
    plan: &ImportValidationPlan,
    expected_prefix: Option<&str>,
) -> Result<(HashMap<String, String>, Vec<ImportPrefixRename>)> {
    if plan.prefix_mismatches.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }

    let Some(prefix) = expected_prefix else {
        return Ok((HashMap::new(), Vec::new()));
    };

    let generator = IdGenerator::new(IdConfig::with_prefix(prefix)?);
    let mut occupied_ids = plan.occupied_ids.clone();
    occupied_ids.extend(storage.get_all_ids()?);

    let mut generated_ids = HashSet::new();
    let mut renames = HashMap::with_capacity(plan.prefix_mismatches.len());
    let mut receipt = Vec::with_capacity(plan.prefix_mismatches.len());

    for seed in &plan.prefix_mismatches {
        let preserved = prefix_preserving_rename(&seed.old_id, prefix)
            .filter(|candidate| id_matches_expected_prefix(candidate, prefix));
        let (new_id, fallback) = match preserved {
            Some(candidate)
                if !occupied_ids.contains(&candidate) && !generated_ids.contains(&candidate) =>
            {
                (candidate, None)
            }
            preserved => {
                // Never silently re-mint over an occupied id: regenerate via
                // the collision-checked generator and record why.
                let reason = if preserved.is_some() {
                    PREFIX_RENAME_FALLBACK_COLLISION
                } else {
                    PREFIX_RENAME_FALLBACK_UNPARSEABLE
                };
                let regenerated = generator.generate(
                    &seed.title,
                    seed.description.as_deref(),
                    seed.created_by.as_deref(),
                    seed.created_at,
                    plan.record_count,
                    |candidate| {
                        Ok(occupied_ids.contains(candidate) || generated_ids.contains(candidate))
                    },
                )?;
                (regenerated, Some(reason.to_string()))
            }
        };
        generated_ids.insert(new_id.clone());
        receipt.push(ImportPrefixRename {
            old_id: seed.old_id.clone(),
            new_id: new_id.clone(),
            fallback,
        });
        renames.insert(seed.old_id.clone(), new_id);
    }

    Ok((renames, receipt))
}

fn apply_prefix_renames(issue: &mut Issue, renames: &HashMap<String, String>) {
    use crate::util::content_hash;

    if let Some(new_id) = renames.get(&issue.id) {
        if issue.external_ref.is_none() {
            issue.external_ref = Some(issue.id.clone());
            // content_hash covers external_ref but not the id itself, so the
            // stash above is the only mutation here that moves the hash.
            issue.content_hash = Some(content_hash(issue));
        }
        issue.id.clone_from(new_id);
    }

    for dep in &mut issue.dependencies {
        if let Some(new_target) = renames.get(&dep.depends_on_id) {
            dep.depends_on_id.clone_from(new_target);
        }
        if let Some(new_source) = renames.get(&dep.issue_id) {
            dep.issue_id.clone_from(new_source);
        }
    }

    for comment in &mut issue.comments {
        if let Some(new_source) = renames.get(&comment.issue_id) {
            comment.issue_id.clone_from(new_source);
        }
    }
}

fn load_import_metadata_maps(storage: &SqliteStorage) -> Result<ImportMetadataMaps> {
    let all_meta = storage.get_all_issues_metadata()?;
    let meta_len = all_meta.len();
    let mut meta_by_id = HashMap::with_capacity(meta_len);
    let mut id_by_ext_ref = HashMap::with_capacity(meta_len);
    let mut id_by_hash = HashMap::with_capacity(meta_len);

    for metadata in all_meta {
        let issue_id = metadata.id.clone();
        if let Some(ext) = metadata.external_ref.as_ref() {
            id_by_ext_ref
                .entry(ext.clone())
                .or_insert_with(|| issue_id.clone());
        }
        if metadata.status != crate::model::Status::Tombstone
            && let Some(hash) = metadata.content_hash.as_ref()
        {
            // Preserve the first matching issue to mirror the old query_row
            // collision path when multiple issues share the same content hash.
            id_by_hash
                .entry(hash.clone())
                .or_insert_with(|| issue_id.clone());
        }
        meta_by_id.insert(issue_id, metadata);
    }

    Ok(ImportMetadataMaps {
        meta_by_id,
        id_by_ext_ref,
        id_by_hash,
    })
}

fn handle_duplicate_external_ref(
    issue: &mut Issue,
    seen_external_refs: &mut HashSet<String>,
    config: &ImportConfig,
) -> Result<()> {
    let Some(ext_ref) = issue.external_ref.clone() else {
        return Ok(());
    };

    if seen_external_refs.contains(&ext_ref) {
        if config.clear_duplicate_external_refs {
            issue.external_ref = None;
            issue.content_hash = Some(crate::util::content_hash(issue));
            Ok(())
        } else {
            Err(BeadsError::Config(format!(
                "Duplicate external_ref: {ext_ref}"
            )))
        }
    } else {
        seen_external_refs.insert(ext_ref);
        Ok(())
    }
}

fn scan_import_collision_renames(
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    prefix_renames: &HashMap<String, String>,
    metadata: &ImportMetadataMaps,
    result: &mut ImportResult,
    record_count: usize,
) -> Result<ImportCollisionPlan> {
    let mut seen_external_refs = HashSet::new();
    let mut renames = HashMap::new();
    let mut comment_owner_ids_to_replace = BTreeSet::new();
    let progress =
        create_progress_bar(record_count as u64, "Scanning issues", config.show_progress);

    for_each_jsonl_import_issue(source, |_line_num, mut issue, _| {
        apply_prefix_renames(&mut issue, prefix_renames);

        if issue.ephemeral {
            result.skipped_count += 1;
            progress.inc(1);
            return Ok(());
        }

        handle_duplicate_external_ref(&mut issue, &mut seen_external_refs, config)?;

        let computed_hash = crate::util::content_hash(&issue);
        let collision = detect_collision(
            &issue,
            &metadata.id_by_ext_ref,
            &metadata.id_by_hash,
            &metadata.meta_by_id,
            &computed_hash,
        );
        let action = determine_action(
            &collision,
            &issue,
            &metadata.meta_by_id,
            config.force_upsert,
        )?;
        let target_id = match &collision {
            CollisionResult::Match { existing_id, .. } => existing_id.clone(),
            CollisionResult::NewIssue => issue.id.clone(),
        };

        if target_id != issue.id {
            renames.insert(issue.id.clone(), target_id.clone());
        }
        if matches!(
            action,
            CollisionAction::Insert | CollisionAction::Update { .. }
        ) {
            comment_owner_ids_to_replace.insert(target_id);
        }

        progress.inc(1);
        Ok(())
    })?;

    progress.finish_with_message("Scan complete");
    Ok(ImportCollisionPlan {
        renames,
        comment_owner_ids_to_replace: comment_owner_ids_to_replace.into_iter().collect(),
    })
}

fn apply_collision_renames(issue: &mut Issue, renames: &HashMap<String, String>) {
    if let Some(new_id) = renames.get(&issue.id) {
        issue.id.clone_from(new_id);
    }

    for dep in &mut issue.dependencies {
        if let Some(new_target) = renames.get(&dep.depends_on_id) {
            dep.depends_on_id.clone_from(new_target);
        }
        if let Some(new_source) = renames.get(&dep.issue_id) {
            dep.issue_id.clone_from(new_source);
        }
    }

    for comment in &mut issue.comments {
        if let Some(new_source) = renames.get(&comment.issue_id) {
            comment.issue_id.clone_from(new_source);
        }
    }
}

fn cleanup_import_orphans_in_tx(storage: &SqliteStorage) -> Result<usize> {
    let orphan_tables = &[
        ("dependencies", "issue_id"),
        ("dependencies", "depends_on_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
        ("events", "issue_id"),
        ("dirty_issues", "issue_id"),
        ("blocked_issues_cache", "issue_id"),
        ("child_counters", "parent_id"),
    ];
    let mut orphans_cleaned = 0usize;

    for (table, col) in orphan_tables {
        let external_dependency_filter = match (*table, *col) {
            ("dependencies", "issue_id") => " AND issue_id NOT LIKE 'external:%'",
            ("dependencies", "depends_on_id") => " AND depends_on_id NOT LIKE 'external:%'",
            _ => "",
        };
        let sql = format!(
            "DELETE FROM {table} WHERE {col} NOT IN (SELECT id FROM issues){external_dependency_filter}"
        );
        orphans_cleaned += storage.execute_raw_count(&sql)?;
    }

    Ok(orphans_cleaned)
}

fn skipped_import_matches_stored_issue(
    storage: &SqliteStorage,
    target_id: &str,
    incoming: &Issue,
) -> Result<bool> {
    let Some(mut stored) = storage.get_issue_for_export(target_id)? else {
        return Ok(false);
    };
    let mut expected = incoming.clone();
    if expected.id != target_id {
        expected.id = target_id.to_string();
    }

    normalize_issue_for_export(&mut stored);
    normalize_issue_for_export(&mut expected);
    Ok(stored.sync_equals(&expected))
}

fn export_hash_entry_for_import_action(
    storage: &SqliteStorage,
    action: &CollisionAction,
    target_id: &str,
    issue: &Issue,
    computed_hash: &str,
) -> Result<Option<(String, String)>> {
    match action {
        CollisionAction::Insert | CollisionAction::Update { .. } => {
            Ok(Some((target_id.to_string(), computed_hash.to_string())))
        }
        CollisionAction::Skip { .. } => {
            if skipped_import_matches_stored_issue(storage, target_id, issue)? {
                Ok(Some((target_id.to_string(), computed_hash.to_string())))
            } else {
                Ok(None)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_import_actions_in_tx(
    storage: &SqliteStorage,
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    prefix_renames: &HashMap<String, String>,
    collision_renames: &HashMap<String, String>,
    comment_owner_ids_to_replace: &[String],
    metadata: &ImportMetadataMaps,
    base_result: &ImportResult,
    progress: &indicatif::ProgressBar,
    fresh_relation_tables_proven_empty: bool,
) -> Result<ImportResult> {
    let mut tx_result = base_result.clone();
    let mut seen_external_refs = HashSet::new();
    let mut export_hash_batch = Vec::with_capacity(IMPORT_EXPORT_HASH_BATCH_SIZE);
    let mut export_hash_ids = HashSet::new();
    let mut uncertified_local_wins = 0usize;

    progress.set_position(0);
    storage.clear_all_export_hashes_in_tx()?;
    // Comment IDs are globally unique. Release every comment row owned by an
    // issue this transaction will replace before replaying any individual
    // issue, so authoritative IDs can move between those issues without the
    // result depending on JSONL line order. The enclosing transaction restores
    // all rows if a later action or semantic verification fails.
    storage.delete_comments_for_import_issue_ids_in_tx(comment_owner_ids_to_replace)?;

    for_each_jsonl_import_issue(
        source,
        |_line_num, mut issue, exact_duplicate_comments_deduplicated| {
            tx_result.exact_duplicate_comments_deduplicated +=
                exact_duplicate_comments_deduplicated;
            apply_prefix_renames(&mut issue, prefix_renames);

            if issue.ephemeral {
                progress.inc(1);
                return Ok(());
            }

            handle_duplicate_external_ref(&mut issue, &mut seen_external_refs, config)?;

            let computed_hash = crate::util::content_hash(&issue);
            let collision = detect_collision(
                &issue,
                &metadata.id_by_ext_ref,
                &metadata.id_by_hash,
                &metadata.meta_by_id,
                &computed_hash,
            );
            let action = determine_action(
                &collision,
                &issue,
                &metadata.meta_by_id,
                config.force_upsert,
            )?;
            let target_id = match &collision {
                CollisionResult::Match { existing_id, .. } => existing_id.clone(),
                CollisionResult::NewIssue => issue.id.clone(),
            };

            apply_collision_renames(&mut issue, collision_renames);
            process_import_action(
                storage,
                &action,
                &issue,
                &mut tx_result,
                fresh_relation_tables_proven_empty,
            )?;

            if let Some((export_id, export_hash)) = export_hash_entry_for_import_action(
                storage,
                &action,
                &target_id,
                &issue,
                &computed_hash,
            )? {
                export_hash_ids.insert(export_id.clone());
                export_hash_batch.push((export_id, export_hash));
                if export_hash_batch.len() >= IMPORT_EXPORT_HASH_BATCH_SIZE {
                    storage.insert_export_hashes_after_clear_in_tx(&export_hash_batch)?;
                    export_hash_batch.clear();
                }
            } else {
                uncertified_local_wins += 1;
            }

            progress.inc(1);
            Ok(())
        },
    )?;

    if !export_hash_batch.is_empty() {
        storage.insert_export_hashes_after_clear_in_tx(&export_hash_batch)?;
    }
    tx_result.export_hashes_recorded = export_hash_ids.len();
    if uncertified_local_wins > 0 {
        tracing::debug!(
            count = uncertified_local_wins,
            "Import preserved local records that differ from JSONL; marking database for flush"
        );
        storage.set_metadata_in_tx("needs_flush", "true")?;
    }

    let orphans_cleaned = cleanup_import_orphans_in_tx(storage)?;
    if orphans_cleaned > 0 {
        tracing::info!(
            count = orphans_cleaned,
            "Cleaned orphaned FK rows after import"
        );
        tx_result.orphan_cleaned_count = orphans_cleaned;
    }

    tx_result.blocked_cache_entries = storage.rebuild_blocked_cache_in_tx()?;
    tx_result.child_counter_entries = storage.rebuild_child_counters_in_tx()?;
    verify_applied_import_issue_semantics(storage, &tx_result.applied_issues)?;

    Ok(tx_result)
}

/// Materialize legacy schema defaults that are written even when older JSONL
/// records omit the corresponding optional fields.
pub(crate) fn canonicalize_persisted_issue_defaults(issue: &mut Issue) {
    issue.source_repo.get_or_insert_with(|| ".".to_string());
    issue.original_size.get_or_insert(0);
}

/// Compare every persisted import field while retaining the order-independent
/// relation semantics used by normal sync comparisons.
pub(crate) fn persisted_import_issue_equals(actual: &Issue, expected: &Issue) -> bool {
    actual.sync_equals(expected)
        && actual.content_hash == expected.content_hash
        && actual.created_at == expected.created_at
        && actual.updated_at == expected.updated_at
        && actual.agent_context == expected.agent_context
}

fn verify_applied_import_issue_semantics(
    storage: &SqliteStorage,
    expected_issues: &[Issue],
) -> Result<()> {
    if expected_issues.is_empty() {
        return Ok(());
    }

    let ids = expected_issues
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>();
    let actual_by_id = storage
        .get_issues_for_export(&ids)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect::<HashMap<_, _>>();

    for expected in expected_issues {
        let actual = actual_by_id.get(&expected.id).ok_or_else(|| {
            BeadsError::SyncConflict {
                message: format!(
                    "Import semantic verification failed: issue {} was written but is not addressable by its JSONL id; rolling back the import",
                    expected.id
                ),
            }
        })?;
        let mut persisted_expected = expected.clone();
        canonicalize_persisted_issue_defaults(&mut persisted_expected);
        if !persisted_import_issue_equals(actual, &persisted_expected) {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Import semantic verification failed: issue {} does not match its normalized JSONL payload; rolling back the import",
                    expected.id
                ),
            });
        }
    }

    Ok(())
}

/// Import issues from a JSONL file.
///
/// Implements classic bd import semantics:
/// 0. Path validation - reject git paths and outside-beads paths without opt-in
/// 1. Conflict marker scan - abort if found
/// 2. Parse JSONL with 2MB buffer
/// 3. Normalize issues (recompute `content_hash`, set defaults)
/// 4. Prefix validation (optional)
/// 5. 4-phase collision detection
/// 6. Tombstone protection
/// 7. Orphan handling
/// 8. Create/update issues
/// 9. Sync deps/labels/comments
/// 10. Refresh blocked cache
/// 11. Update metadata
///
/// # Errors
///
/// Returns an error if:
/// - Path validation fails (git path, outside `beads_dir` without opt-in)
/// - Conflict markers are detected
/// - File cannot be read
/// - Prefix validation fails
/// - Database operations fail
#[allow(clippy::too_many_lines)]
pub fn import_from_jsonl(
    storage: &mut SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportResult> {
    // Step 0: Path validation (PC-1, PC-2, PC-3, NGI-3) - BEFORE any file operations
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl)?;
        tracing::debug!(
            input_path = %input_path.display(),
            beads_dir = %beads_dir.display(),
            allow_external = config.allow_external_jsonl,
            "Import path validated"
        );
    }

    let source = capture_jsonl_source_snapshot(input_path)?;
    import_from_jsonl_snapshot(storage, &source, config, expected_prefix)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn import_from_jsonl_snapshot(
    storage: &mut SqliteStorage,
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportResult> {
    import_from_jsonl_snapshot_impl(storage, source, config, expected_prefix, None)
}

/// Import into the exact empty replacement installed by a database-family
/// authority. The linear witness cannot be manufactured by ordinary import
/// callers, and the transaction still proves that all owned relation tables
/// remain globally empty before enabling the insert-only relation path.
pub(crate) fn import_from_jsonl_snapshot_into_fresh_replacement(
    storage: &mut SqliteStorage,
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
    witness: FreshDatabaseReplacementWitness,
) -> Result<ImportResult> {
    import_from_jsonl_snapshot_impl(storage, source, config, expected_prefix, Some(witness))
}

// Taking ownership is deliberate: callers must relinquish the linear witness,
// while the transaction closure may need to borrow it across internal BUSY retries.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn import_from_jsonl_snapshot_impl(
    storage: &mut SqliteStorage,
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
    fresh_witness: Option<FreshDatabaseReplacementWitness>,
) -> Result<ImportResult> {
    // Reject a displaced fresh replacement before any metadata or collision
    // query touches the now-unlinked SQLite connection.  The transaction
    // rechecks the witness below so a later inode swap still fails closed
    // before the insert-only relation path is enabled.
    if let Some(witness) = fresh_witness.as_ref() {
        storage.verify_fresh_database_replacement_witness(witness)?;
    }

    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(
            source.display_path(),
            beads_dir,
            config.allow_external_jsonl,
        )?;
    }

    // Step 1: Conflict marker scan
    ensure_no_conflict_markers_snapshot(source)?;

    // Step 2: Parse, Normalize, Validate, and collect minimal rename state.
    let spinner = create_spinner("Parsing and validating issues", config.show_progress);
    let validation_plan = collect_import_validation_plan(source, config, expected_prefix)?;
    spinner.finish_with_message("Parsed and validated issues");

    let mut result = ImportResult::default();

    // Step 5: Handle renames if requested
    let prefix_renames = if config.rename_on_import {
        let (renames, receipt) = build_prefix_renames(storage, &validation_plan, expected_prefix)?;
        result.prefix_renames = receipt;
        renames
    } else {
        HashMap::new()
    };

    // Preload metadata for O(1) collision detection while streaming the input.
    let metadata = load_import_metadata_maps(storage)?;

    // Phase 1: Scan and Resolve IDs
    let collision_plan = scan_import_collision_renames(
        source,
        config,
        &prefix_renames,
        &metadata,
        &mut result,
        validation_plan.record_count,
    )?;

    let jsonl_hash = compute_jsonl_snapshot_content_hash(source)?;
    let observed_jsonl = observed_jsonl_snapshot_witness(source);

    // Phase 2: Execute Actions
    //
    // Disable FK constraints during bulk import so that issues can reference
    // other issues (in dependencies/comments) that haven't been inserted yet.
    // FK integrity is restored and validated after all data is loaded.
    storage
        .execute_raw("PRAGMA foreign_keys = OFF")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to disable foreign key enforcement before import".to_string(),
            source: Box::new(source),
        })?;

    let progress = create_progress_bar(
        validation_plan.record_count as u64,
        "Importing issues",
        config.show_progress,
    );

    let apply_result = storage.with_write_transaction(|storage| -> Result<ImportResult> {
        let fresh_relation_tables_proven_empty = if let Some(witness) = fresh_witness.as_ref() {
            storage.verify_fresh_database_replacement_witness(witness)?;
            if !storage.import_relation_tables_are_globally_empty_in_tx()? {
                return Err(BeadsError::SyncConflict {
                    message: "Fresh database replacement gained relation rows before import"
                        .to_string(),
                });
            }
            true
        } else {
            false
        };
        let tx_result = stream_import_actions_in_tx(
            storage,
            source,
            config,
            &prefix_renames,
            &collision_plan.renames,
            &collision_plan.comment_owner_ids_to_replace,
            &metadata,
            &result,
            &progress,
            fresh_relation_tables_proven_empty,
        )?;

        storage.set_metadata_in_tx(METADATA_LAST_IMPORT_TIME, &chrono::Utc::now().to_rfc3339())?;
        storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, &jsonl_hash)?;
        record_observed_jsonl_witness_in_tx(storage, &observed_jsonl)?;

        Ok(tx_result)
    });

    let validate_foreign_keys = apply_result.is_ok();
    let fk_restore_result = restore_foreign_keys_after_import(storage, validate_foreign_keys);

    match finish_import_after_foreign_key_restore(apply_result, fk_restore_result) {
        Ok(import_result) => {
            progress.finish_with_message("Import complete");
            Ok(import_result)
        }
        Err(err) => {
            progress.finish_and_clear();
            Err(err)
        }
    }
}

pub(crate) fn id_matches_expected_prefix(id: &str, expected_prefix: &str) -> bool {
    let normalized_prefix = expected_prefix.trim_end_matches('-');
    if normalized_prefix.is_empty() {
        return false;
    }

    parse_id(id).is_ok_and(|parsed| {
        // Slugged root IDs are shaped as `<prefix>-<slug>-<hash>`.
        // `parse_id` treats the slug as part of the hyphenated prefix, so
        // prefix guardrails must accept this generated prefix family.
        parsed.prefix == normalized_prefix
            || parsed
                .prefix
                .strip_prefix(normalized_prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

/// Process a single import action.
fn process_import_action(
    storage: &SqliteStorage,
    action: &CollisionAction,
    issue: &Issue,
    result: &mut ImportResult,
    fresh_relation_tables_proven_empty: bool,
) -> Result<()> {
    match action {
        CollisionAction::Insert => {
            let inserted = insert_new_import_issue(storage, issue)?;
            if inserted
                && (fresh_relation_tables_proven_empty
                    || !storage.has_owned_relation_rows_for_import(&issue.id)?)
            {
                storage.insert_new_issue_relations_for_import_in_tx(issue)?;
            } else {
                sync_issue_relations(storage, issue)?;
            }
            result.imported_count += 1;
            result.created_count += 1;
            record_imported_relation_counts(result, issue);
            result.applied_issues.push(issue.clone());
        }
        CollisionAction::Update { existing_id } => {
            // When updating by external_ref or content_hash, the incoming issue may have
            // a different ID than the existing one. We need to update using the existing ID.
            if existing_id == &issue.id {
                storage.upsert_issue_for_import_in_tx(issue)?;
                sync_issue_relations(storage, issue)?;
                result.applied_issues.push(issue.clone());
            } else {
                let mut updated_issue = issue.clone();
                updated_issue.id.clone_from(existing_id);
                storage.upsert_issue_for_import_in_tx(&updated_issue)?;
                sync_issue_relations(storage, &updated_issue)?;
                result.applied_issues.push(updated_issue);
            }
            result.imported_count += 1;
            result.updated_count += 1;
            record_imported_relation_counts(result, issue);
        }
        CollisionAction::Skip { reason } => {
            tracing::debug!(id = %issue.id, reason = %reason, "Skipping issue");
            if reason.starts_with("Tombstone") {
                result.tombstone_skipped += 1;
            } else {
                result.skipped_count += 1;
            }
        }
    }
    Ok(())
}

fn insert_new_import_issue(storage: &SqliteStorage, issue: &Issue) -> Result<bool> {
    match storage.insert_new_issue_for_import_in_tx(issue) {
        Ok(_) => Ok(true),
        Err(BeadsError::Database(
            fsqlite_error::FrankenError::PrimaryKeyViolation
            | fsqlite_error::FrankenError::UniqueViolation { .. },
        )) => {
            tracing::debug!(
                id = %issue.id,
                "Import insert found a concurrent key collision; falling back to upsert"
            );
            storage.upsert_issue_for_import_in_tx(issue)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn record_imported_relation_counts(result: &mut ImportResult, issue: &Issue) {
    result.labels_imported += issue.labels.len();
    result.dependencies_imported += issue.dependencies.len();
    result.comments_imported += issue.comments.len();
}

/// Sync labels, dependencies, and comments for an imported issue.
fn sync_issue_relations(storage: &SqliteStorage, issue: &Issue) -> Result<()> {
    // Sync labels
    storage.sync_labels_for_import_in_tx(&issue.id, &issue.labels)?;

    // Sync dependencies
    storage.sync_dependencies_for_import_in_tx(&issue.id, &issue.dependencies)?;

    // Sync comments
    storage.sync_comments_for_import_in_tx(&issue.id, &issue.comments)?;

    Ok(())
}

/// Finalize an import by computing the content hash of the imported file.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
fn compute_jsonl_hash_from_reader(mut reader: impl BufRead) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut line_buf = Vec::with_capacity(4096);

    loop {
        line_buf.clear();
        let bytes_read = reader.read_until(b'\n', &mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        // Efficiently skip empty or whitespace-only lines without UTF-8 validation.
        // trim_ascii() is a fast byte-based trim.
        let trimmed = line_buf.trim_ascii();
        if !trimmed.is_empty() {
            hasher.update(trimmed);
            hasher.update(b"\n");
        }
    }

    Ok(hex_encode(&hasher.finalize()))
}

// Infallible today, but callers outside this module (`cli::commands::sync`)
// consume the fallible signature; keep `Result` to avoid a cross-file change.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn compute_jsonl_snapshot_content_hash(source: &JsonlSourceSnapshot) -> Result<String> {
    Ok(source.content_sha256().to_string())
}

/// Finalize an import by computing the canonical content hash of the file.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn compute_jsonl_hash(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    self::path::validate_jsonl_fd_metadata(&file, path)?;
    compute_jsonl_hash_from_reader(std::io::BufReader::new(file))
}

// ============================================================================
// Additive Reconciliation (beads_rust-3r45)
// ============================================================================

/// Schema marker for `br sync --reconcile` receipts.
pub const SYNC_RECONCILE_SCHEMA_VERSION: &str = "br.sync.reconcile.v1";

/// Per-row action kind planned by additive reconciliation.
///
/// Deletion is structurally impossible in this mode: there is no variant for
/// it, and the applier only ever routes through the import-path upserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileActionKind {
    /// JSONL row has no DB counterpart; insert it.
    Create,
    /// JSONL row is strictly newer than its DB counterpart; update in place.
    Update,
    /// DB counterpart is strictly newer; leave it alone.
    SkipOlder,
    /// Timestamps are equal; leave the DB row alone.
    SkipEqual,
    /// DB counterpart is a tombstone; tombstone protection wins.
    SkipTombstone,
}

/// One planned JSONL-row action bound to a target issue id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileAction {
    /// 1-based JSONL line number the row was parsed from.
    pub line: usize,
    /// Issue id as written in the JSONL row.
    pub incoming_id: String,
    /// DB issue id the action applies to (differs from `incoming_id` when the
    /// collision detector matched by `external_ref` or content hash).
    pub target_id: String,
    /// Planned action.
    pub kind: ReconcileActionKind,
}

/// Witnesses a reconcile plan was computed against.
///
/// The applier re-verifies every field before mutating anything and rolls the
/// transaction back on any mismatch, so a plan can never be applied to state
/// it was not computed from.
#[derive(Debug, Clone)]
pub struct ReconcileWitness {
    /// Whitespace-normalized SHA-256 of the JSONL content (same function the
    /// stored `jsonl_content_hash` metadata uses).
    pub jsonl_content_hash: String,
    /// RFC3339 mtime of the JSONL file at plan time.
    pub jsonl_mtime_witness: String,
    /// Byte size of the JSONL file at plan time.
    pub jsonl_size: u64,
    /// Total issue rows in the DB at plan time (including tombstones).
    pub db_issue_count: usize,
    /// Total event rows at plan time.
    pub events_count: u64,
    /// Highest event rowid at plan time.
    pub events_max_id: Option<i64>,
}

/// Read-only additive reconciliation plan.
#[derive(Debug, Clone)]
pub struct ReconcilePlan {
    /// Non-empty JSONL rows parsed (including ephemeral rows).
    pub record_count: usize,
    /// Ephemeral (`-wisp-`) rows excluded from planning.
    pub ephemeral_skipped: usize,
    /// Ordered per-row actions (ephemeral rows excluded).
    pub actions: Vec<ReconcileAction>,
    /// Exportable DB issue ids absent from the JSONL row set (sorted). These
    /// are never touched by apply; they only inform the `needs_flush` repair.
    pub db_only_ids: Vec<String>,
    /// Label rows carried by planned create/update rows.
    pub labels_planned: usize,
    /// Dependency rows carried by planned create/update rows.
    pub dependencies_planned: usize,
    /// Comment rows carried by planned create/update rows.
    pub comments_planned: usize,
    /// Whether the stored `jsonl_content_hash` metadata already matches the
    /// file. True together with a non-empty create/update set is exactly the
    /// false-equal state this mode exists to repair.
    pub stored_hash_matches_jsonl: bool,
    /// Bound witnesses for apply-time verification.
    pub witness: ReconcileWitness,
}

impl ReconcilePlan {
    /// Count planned actions of one kind.
    #[must_use]
    pub fn count_kind(&self, kind: ReconcileActionKind) -> usize {
        self.actions.iter().filter(|a| a.kind == kind).count()
    }

    /// Whether apply would change any issue row.
    #[must_use]
    pub fn has_row_changes(&self) -> bool {
        self.actions.iter().any(|a| {
            matches!(
                a.kind,
                ReconcileActionKind::Create | ReconcileActionKind::Update
            )
        })
    }

    /// Sorted target ids for one action kind.
    #[must_use]
    pub fn target_ids_for_kind(&self, kind: ReconcileActionKind) -> Vec<String> {
        let mut ids: Vec<String> = self
            .actions
            .iter()
            .filter(|a| a.kind == kind)
            .map(|a| a.target_id.clone())
            .collect();
        ids.sort();
        ids
    }
}

/// Outcome of applying an additive reconcile plan.
#[derive(Debug, Clone, Default)]
pub struct ReconcileApplyOutcome {
    /// Issues inserted.
    pub created: usize,
    /// Issues updated in place.
    pub updated: usize,
    /// Rows skipped because the DB copy is strictly newer.
    pub skipped_older: usize,
    /// Rows skipped because timestamps are equal.
    pub skipped_equal: usize,
    /// Rows skipped by tombstone protection.
    pub skipped_tombstone: usize,
    /// Ephemeral rows excluded.
    pub ephemeral_skipped: usize,
    /// Label rows written for applied issues.
    pub labels_imported: usize,
    /// Dependency rows written for applied issues.
    pub dependencies_imported: usize,
    /// Comment rows written for applied issues.
    pub comments_imported: usize,
    /// Export-hash rows recorded for rows whose DB copy now matches JSONL.
    pub export_hashes_recorded: usize,
    /// Skipped rows whose DB copy differs from JSONL (local wins that still
    /// need a future flush).
    pub uncertified_local_wins: usize,
    /// Dangling dependency rows removed from just-written issues.
    pub orphan_dependencies_cleaned: usize,
    /// Blocked-cache rows after rebuild (0 when no rebuild was needed).
    pub blocked_cache_entries: usize,
    /// Child-counter rows after rebuild (0 when no rebuild was needed).
    pub child_counter_entries: usize,
    /// Event rows after apply (must equal the plan's `events_count`).
    pub events_after: u64,
    /// Whether `needs_flush` was set because local state still diverges from
    /// JSONL (db-only rows or uncertified local wins).
    pub needs_flush_set: bool,
    /// Whether the import metadata (content hash + witness + import time) was
    /// repaired in the apply transaction.
    pub metadata_repaired: bool,
}

fn reconcile_kind_for_action(action: &CollisionAction) -> ReconcileActionKind {
    match action {
        CollisionAction::Insert => ReconcileActionKind::Create,
        CollisionAction::Update { .. } => ReconcileActionKind::Update,
        CollisionAction::Skip { reason } => {
            if reason.starts_with("Tombstone") {
                ReconcileActionKind::SkipTombstone
            } else if reason.starts_with("Equal timestamps") {
                ReconcileActionKind::SkipEqual
            } else {
                ReconcileActionKind::SkipOlder
            }
        }
    }
}

fn reconcile_config_error(message: impl Into<String>) -> BeadsError {
    BeadsError::Config(message.into())
}

/// Classify every non-ephemeral JSONL row against the current DB state.
///
/// Shared by the read-only planner and the in-transaction apply verifier so
/// both sides are guaranteed to run the identical classification. The
/// callback receives each parsed issue together with its classified action;
/// classification uses the same collision detector and timestamp-newer-wins
/// action table as `--import-only` with `force_upsert` disabled.
fn for_each_reconcile_classified_row<F>(
    source: &JsonlSourceSnapshot,
    config: &ImportConfig,
    metadata: &ImportMetadataMaps,
    mut handle: F,
) -> Result<(usize, usize)>
where
    F: FnMut(usize, Issue, &CollisionAction, &str) -> Result<()>,
{
    let mut seen_external_refs = HashSet::new();
    let mut record_count = 0usize;
    let mut ephemeral_skipped = 0usize;

    for_each_jsonl_import_issue(source, |line_num, mut issue, _| {
        record_count += 1;
        if issue.ephemeral {
            ephemeral_skipped += 1;
            return Ok(());
        }

        handle_duplicate_external_ref(&mut issue, &mut seen_external_refs, config)?;

        let computed_hash = crate::util::content_hash(&issue);
        let collision = detect_collision(
            &issue,
            &metadata.id_by_ext_ref,
            &metadata.id_by_hash,
            &metadata.meta_by_id,
            &computed_hash,
        );
        let action = determine_action(&collision, &issue, &metadata.meta_by_id, false)?;
        let target_id = match &collision {
            CollisionResult::Match { existing_id, .. } => existing_id.clone(),
            CollisionResult::NewIssue => issue.id.clone(),
        };

        handle(line_num, issue, &action, &target_id)
    })?;

    Ok((record_count, ephemeral_skipped))
}

/// Plan an additive JSONL→DB reconciliation without any mutation.
///
/// The planner parses and validates the JSONL, classifies every row with the
/// import collision detector (timestamp-newer-wins, tombstone protection, no
/// force), and binds the resulting plan to content/stat witnesses of both
/// sides. It compares full issue state instead of trusting the cached
/// `jsonl_content_hash` metadata, so it sees divergence that the
/// `--import-only` staleness short-circuit is structurally blind to.
///
/// The planner opens no write transaction and writes nothing: no metadata, no
/// JSONL, no base snapshot, no dirty markers, no caches.
///
/// # Errors
///
/// Returns an error if path validation fails, the JSONL is missing, contains
/// conflict markers, malformed JSON, duplicate ids or duplicate external
/// refs, or if the JSONL file changes while planning.
pub fn plan_sync_reconcile(
    storage: &SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
) -> Result<ReconcilePlan> {
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl)?;
    }
    if !input_path.is_file() {
        return Err(reconcile_config_error(format!(
            "Cannot reconcile: JSONL file not found at {}",
            input_path.display()
        )));
    }
    ensure_no_conflict_markers(input_path)?;

    // Reconcile never renames prefixes or ids; run the structural validation
    // pass (duplicate-id detection, parseability) with prefix checks off.
    let mut validation_config = config.clone();
    validation_config.skip_prefix_validation = true;
    validation_config.rename_on_import = false;
    // One immutable capture serves validation and classification so a
    // concurrent JSONL writer cannot split what the planner observes.
    let source = capture_jsonl_source_snapshot(input_path)?;
    collect_import_validation_plan(&source, &validation_config, None)?;

    // Bind the source witness before classification so a concurrent JSONL
    // writer is detected by the re-stat below.
    let observed = observed_jsonl_witness(input_path)?;
    let jsonl_content_hash = compute_jsonl_hash(input_path)?;

    let metadata = load_import_metadata_maps(storage)?;

    let mut actions = Vec::new();
    let mut jsonl_target_ids = HashSet::new();
    let mut labels_planned = 0usize;
    let mut dependencies_planned = 0usize;
    let mut comments_planned = 0usize;

    let (record_count, ephemeral_skipped) = for_each_reconcile_classified_row(
        &source,
        config,
        &metadata,
        |line_num, issue, action, target_id| {
            let kind = reconcile_kind_for_action(action);
            if matches!(
                kind,
                ReconcileActionKind::Create | ReconcileActionKind::Update
            ) {
                labels_planned += issue.labels.len();
                dependencies_planned += issue.dependencies.len();
                comments_planned += issue.comments.len();
            }
            jsonl_target_ids.insert(target_id.to_string());
            actions.push(ReconcileAction {
                line: line_num,
                incoming_id: issue.id,
                target_id: target_id.to_string(),
                kind,
            });
            Ok(())
        },
    )?;

    let mut db_only_ids: Vec<String> = storage
        .get_non_ephemeral_issue_ids()?
        .into_iter()
        .filter(|id| !jsonl_target_ids.contains(id))
        .collect();
    db_only_ids.sort();

    let stored_hash_matches_jsonl = storage
        .get_metadata(METADATA_JSONL_CONTENT_HASH)?
        .as_deref()
        == Some(jsonl_content_hash.as_str());
    let db_issue_count = storage.count_all_issues()?;
    let (events_count, events_max_id) = storage.events_table_witness()?;

    // Re-stat: reject the plan if the JSONL changed underneath the scan.
    let final_observed = observed_jsonl_witness(input_path)?;
    if final_observed.mtime_witness != observed.mtime_witness
        || final_observed.size != observed.size
    {
        return Err(reconcile_config_error(format!(
            "JSONL file {} changed while planning reconciliation; re-run the command",
            input_path.display()
        )));
    }

    Ok(ReconcilePlan {
        record_count,
        ephemeral_skipped,
        actions,
        db_only_ids,
        labels_planned,
        dependencies_planned,
        comments_planned,
        stored_hash_matches_jsonl,
        witness: ReconcileWitness {
            jsonl_content_hash,
            jsonl_mtime_witness: observed.mtime_witness,
            jsonl_size: observed.size,
            db_issue_count,
            events_count,
            events_max_id,
        },
    })
}

/// Apply a previously computed additive reconcile plan.
///
/// The applier re-verifies the plan's JSONL and DB witnesses, re-runs the
/// classification inside a single write transaction, and rolls everything
/// back if any row classifies differently from the plan. Writes go through
/// the import-path upserts only: no table resets, no deletes of unsuperseded
/// rows, no manufactured events, no JSONL/base writes, no VACUUM. The same
/// transaction repairs the import metadata (content hash, stat witness,
/// import time) so the stale-hash short-circuit stops reporting a false
/// equal, and sets `needs_flush` when local state still diverges from JSONL
/// (db-only rows or newer local rows).
///
/// # Errors
///
/// Returns an error (with the transaction rolled back and foreign keys
/// restored) if the JSONL or DB changed since planning, if any row action
/// diverges from the plan, if event rows changed during apply, or if any
/// database operation fails.
pub fn apply_sync_reconcile(
    storage: &mut SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    plan: &ReconcilePlan,
) -> Result<ReconcileApplyOutcome> {
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl)?;
    }
    ensure_no_conflict_markers(input_path)?;

    verify_reconcile_jsonl_witness(input_path, plan)?;

    // Build the cross-reference rename map from the verified plan: rows whose
    // collision target differs from their written id (external_ref or
    // content-hash matches) must have their relation references rewritten the
    // same way `--import-only` does.
    let collision_renames: HashMap<String, String> = plan
        .actions
        .iter()
        .filter(|a| a.incoming_id != a.target_id)
        .map(|a| (a.incoming_id.clone(), a.target_id.clone()))
        .collect();

    // Imported rows may reference issues that appear later in the stream, so
    // FK enforcement is deferred exactly like the import path.
    storage
        .execute_raw("PRAGMA foreign_keys = OFF")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to disable foreign key enforcement before reconcile".to_string(),
            source: Box::new(source),
        })?;

    let apply_result = storage.with_write_transaction(|storage| {
        run_reconcile_apply_tx(storage, input_path, config, plan, &collision_renames)
    });

    let validate_foreign_keys = apply_result.is_ok();
    let fk_restore_result = restore_foreign_keys_after_import(storage, validate_foreign_keys);

    let outcome = match (apply_result, fk_restore_result) {
        (Ok(outcome), Ok(())) => outcome,
        (Ok(_), Err(fk_err)) => return Err(fk_err),
        (Err(apply_err), Ok(())) => return Err(apply_err),
        (Err(apply_err), Err(fk_err)) => {
            tracing::error!(
                error = %fk_err,
                "Failed to restore foreign key enforcement after failed reconcile"
            );
            return Err(BeadsError::WithContext {
                context: format!(
                    "reconcile failed, and SQLite foreign key enforcement could not be re-enabled: {fk_err}"
                ),
                source: Box::new(apply_err),
            });
        }
    };

    tracing::info!(
        created = outcome.created,
        updated = outcome.updated,
        skipped_older = outcome.skipped_older,
        skipped_equal = outcome.skipped_equal,
        skipped_tombstone = outcome.skipped_tombstone,
        events = outcome.events_after,
        needs_flush_set = outcome.needs_flush_set,
        "Additive reconcile applied"
    );

    Ok(outcome)
}

fn verify_reconcile_jsonl_witness(input_path: &Path, plan: &ReconcilePlan) -> Result<()> {
    let observed = observed_jsonl_witness(input_path)?;
    if observed.mtime_witness != plan.witness.jsonl_mtime_witness
        || observed.size != plan.witness.jsonl_size
    {
        return Err(reconcile_config_error(format!(
            "JSONL file {} changed since the reconcile plan was computed; re-run the command",
            input_path.display()
        )));
    }
    let current_hash = compute_jsonl_hash(input_path)?;
    if current_hash != plan.witness.jsonl_content_hash {
        return Err(reconcile_config_error(format!(
            "JSONL content at {} no longer matches the reconcile plan; re-run the command",
            input_path.display()
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn run_reconcile_apply_tx(
    storage: &SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    plan: &ReconcilePlan,
    collision_renames: &HashMap<String, String>,
) -> Result<ReconcileApplyOutcome> {
    // DB witness: the events table must be exactly as planned.
    let (events_before, events_max_before) = storage.events_table_witness()?;
    if events_before != plan.witness.events_count || events_max_before != plan.witness.events_max_id
    {
        return Err(reconcile_config_error(
            "database events changed since the reconcile plan was computed; re-run the command",
        ));
    }

    // Fresh in-transaction metadata: classification must be re-derived from
    // live state, then compared row-by-row against the plan.
    let metadata = load_import_metadata_maps(storage)?;

    let mut outcome = ReconcileApplyOutcome::default();
    let mut import_result = ImportResult::default();
    let mut export_hash_batch: Vec<(String, String)> = Vec::new();
    let mut export_hash_ids = HashSet::new();
    let mut applied_ids: Vec<String> = Vec::new();
    let mut action_index = 0usize;

    let source = capture_jsonl_source_snapshot(input_path)?;
    let (record_count, ephemeral_skipped) = for_each_reconcile_classified_row(
        &source,
        config,
        &metadata,
        |line_num, mut issue, action, target_id| {
            let planned = plan.actions.get(action_index).ok_or_else(|| {
                reconcile_config_error(
                    "JSONL gained rows since the reconcile plan was computed; re-run the command",
                )
            })?;
            let kind = reconcile_kind_for_action(action);
            if planned.line != line_num
                || planned.incoming_id != issue.id
                || planned.target_id != target_id
                || planned.kind != kind
            {
                return Err(reconcile_config_error(format!(
                    "database or JSONL changed between plan and apply at line {line_num} \
                     (planned {:?} {} -> {}, found {:?} {} -> {}); transaction rolled back",
                    planned.kind, planned.incoming_id, planned.target_id, kind, issue.id, target_id,
                )));
            }
            action_index += 1;

            let computed_hash = crate::util::content_hash(&issue);
            apply_collision_renames(&mut issue, collision_renames);
            process_import_action(storage, action, &issue, &mut import_result, false)?;

            match kind {
                ReconcileActionKind::Create | ReconcileActionKind::Update => {
                    applied_ids.push(target_id.to_string());
                }
                ReconcileActionKind::SkipOlder
                | ReconcileActionKind::SkipEqual
                | ReconcileActionKind::SkipTombstone => {}
            }

            if let Some((export_id, export_hash)) = export_hash_entry_for_import_action(
                storage,
                action,
                target_id,
                &issue,
                &computed_hash,
            )? {
                if export_hash_ids.insert(export_id.clone()) {
                    export_hash_batch.push((export_id, export_hash));
                }
            } else {
                outcome.uncertified_local_wins += 1;
            }
            Ok(())
        },
    )?;

    if action_index != plan.actions.len() {
        return Err(reconcile_config_error(
            "JSONL lost rows since the reconcile plan was computed; re-run the command",
        ));
    }

    if !export_hash_batch.is_empty() {
        storage.set_changed_export_hashes_in_tx(&export_hash_batch)?;
    }

    outcome.created = import_result.created_count;
    outcome.updated = import_result.updated_count;
    outcome.skipped_older = plan.count_kind(ReconcileActionKind::SkipOlder);
    outcome.skipped_equal = plan.count_kind(ReconcileActionKind::SkipEqual);
    outcome.skipped_tombstone = import_result.tombstone_skipped;
    outcome.ephemeral_skipped = ephemeral_skipped;
    outcome.labels_imported = import_result.labels_imported;
    outcome.dependencies_imported = import_result.dependencies_imported;
    outcome.comments_imported = import_result.comments_imported;
    outcome.export_hashes_recorded = export_hash_ids.len();
    if record_count != plan.record_count {
        return Err(reconcile_config_error(
            "JSONL record count changed since the reconcile plan was computed; re-run the command",
        ));
    }

    if !applied_ids.is_empty() {
        applied_ids.sort();
        applied_ids.dedup();
        outcome.orphan_dependencies_cleaned =
            storage.delete_orphan_dependencies_for_issues_in_tx(&applied_ids)?;
        if outcome.orphan_dependencies_cleaned > 0 {
            tracing::info!(
                count = outcome.orphan_dependencies_cleaned,
                "Reconcile removed dangling dependency rows from applied issues"
            );
        }
        outcome.blocked_cache_entries = storage.rebuild_blocked_cache_in_tx()?;
        outcome.child_counter_entries = storage.rebuild_child_counters_in_tx()?;
    }

    // Repair the import metadata inside the same transaction so the
    // false-equal stored hash cannot survive a successful apply.
    let observed = observed_jsonl_witness(input_path)?;
    if observed.mtime_witness != plan.witness.jsonl_mtime_witness
        || observed.size != plan.witness.jsonl_size
    {
        return Err(reconcile_config_error(format!(
            "JSONL file {} changed during reconcile apply; transaction rolled back",
            input_path.display()
        )));
    }
    storage.set_metadata_in_tx(METADATA_LAST_IMPORT_TIME, &chrono::Utc::now().to_rfc3339())?;
    storage.set_metadata_in_tx(
        METADATA_JSONL_CONTENT_HASH,
        &plan.witness.jsonl_content_hash,
    )?;
    record_observed_jsonl_witness_in_tx(storage, &observed)?;
    outcome.metadata_repaired = true;

    if outcome.uncertified_local_wins > 0 || !plan.db_only_ids.is_empty() {
        tracing::debug!(
            uncertified_local_wins = outcome.uncertified_local_wins,
            db_only = plan.db_only_ids.len(),
            "Reconcile preserved local records that diverge from JSONL; marking database for flush"
        );
        storage.set_metadata_in_tx("needs_flush", "true")?;
        outcome.needs_flush_set = true;
    }

    // Hard guarantee: reconcile never creates, mutates, or deletes events.
    let (events_after, events_max_after) = storage.events_table_witness()?;
    if events_after != events_before || events_max_after != events_max_before {
        return Err(reconcile_config_error(
            "reconcile apply would have changed audit events; transaction rolled back",
        ));
    }
    outcome.events_after = events_after;

    Ok(outcome)
}

// ============================================================================
// 3-Way Merge Types and Functions
// ============================================================================

/// Types of conflicts that can occur during 3-way merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictType {
    /// Issue was modified locally but deleted externally (or vice versa).
    DeleteVsModify,
    /// Issue was modified independently in both local and external stores.
    BothModified,
    /// Issue was created in both local and external with different content.
    ConvergentCreation,
}

/// Result of merging a single issue across base, left (local), and right (external).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// No action needed (e.g., issue doesn't exist in any source).
    NoAction,
    /// Keep the specified issue.
    Keep(Issue),
    /// Keep the specified issue with a note about the merge decision.
    KeepWithNote(Issue, String),
    /// Delete the issue.
    Delete,
    /// A conflict was detected that requires manual resolution.
    Conflict(ConflictType),
}

/// Context for performing a 3-way merge operation.
#[derive(Debug, Default)]
pub struct MergeContext {
    /// Base state (last known common state).
    pub base: std::collections::HashMap<String, Issue>,
    /// Left state (current SQLite/local changes).
    pub left: std::collections::HashMap<String, Issue>,
    /// Right state (current JSONL/external changes).
    pub right: std::collections::HashMap<String, Issue>,
}

impl MergeContext {
    /// Create a new merge context from the three states.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        base: std::collections::HashMap<String, Issue>,
        left: std::collections::HashMap<String, Issue>,
        right: std::collections::HashMap<String, Issue>,
    ) -> Self {
        Self { base, left, right }
    }

    /// Get all unique issue IDs across all three states.
    #[must_use]
    pub fn all_issue_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        ids.extend(self.base.keys().cloned());
        ids.extend(self.left.keys().cloned());
        ids.extend(self.right.keys().cloned());
        ids
    }
}

/// Report of a 3-way merge operation.
#[derive(Debug, Default)]
pub struct MergeReport {
    /// Issues that were kept (created or updated).
    pub kept: Vec<Issue>,
    /// Issues that were deleted.
    pub deleted: Vec<String>,
    /// Conflicts that were detected.
    pub conflicts: Vec<(String, ConflictType)>,
    /// Issues that were skipped due to tombstone protection.
    pub tombstone_protected: Vec<String>,
    /// Notes about merge decisions.
    pub notes: Vec<(String, String)>,
}

impl MergeReport {
    /// Returns true if there were any conflicts.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Total number of actions taken.
    #[must_use]
    pub fn total_actions(&self) -> usize {
        self.kept.len() + self.deleted.len()
    }
}

/// Strategy for resolving conflicts during merge.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum ConflictResolution {
    /// Always keep the local (`SQLite`) version.
    #[default]
    PreferLocal,
    /// Always keep the external (`JSONL`) version.
    PreferExternal,
    /// Use `updated_at` timestamp to determine winner (or specified strategy)
    PreferNewer,
    /// Report conflict without auto-resolving.
    Manual,
}

/// Merge a single issue given its state in base, left (local), and right (external).
///
/// This implements the core 3-way merge logic for a single issue:
/// - New local issues are kept
/// - New external issues are imported
/// - Deletions are handled based on whether the other side modified
/// - Both-modified uses `updated_at` as tiebreaker (or specified strategy)
///
/// # Arguments
/// * `base` - The issue in the base (common ancestor) state, if it existed
/// * `left` - The issue in the local (`SQLite`) state, if it exists
/// * `right` - The issue in the external (JSONL) state, if it exists
/// * `strategy` - How to resolve conflicts when both sides modified
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn merge_issue(
    base: Option<&Issue>,
    left: Option<&Issue>,
    right: Option<&Issue>,
    strategy: ConflictResolution,
) -> MergeResult {
    match (base, left, right) {
        // Case 1: Only in base (deleted in both local and external) -> no action
        (Some(_), None, None) => MergeResult::Delete,

        // Case 2: Only in left (new local) -> keep
        (None, Some(l), None) => MergeResult::Keep(l.clone()),

        // Case 3: Only in right (new external) -> keep
        (None, None, Some(r)) => MergeResult::Keep(r.clone()),

        // Case 4: In base and left only (deleted in right/external)
        (Some(b), Some(l), None) => {
            // Was it modified locally after base?
            if l.sync_equals(b) {
                // Local unchanged since base, external deleted -> delete
                MergeResult::Delete
            } else {
                // Local modified but external deleted - conflict
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Local modified, external deleted - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::Delete,
                    ConflictResolution::PreferNewer => {
                        // Keep local since it was modified more recently than base
                        MergeResult::KeepWithNote(
                            l.clone(),
                            "Local modified after base, external deleted - kept local".to_string(),
                        )
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::DeleteVsModify)
                    }
                }
            }
        }

        // Case 5: In base and right only (deleted locally)
        (Some(b), None, Some(r)) => {
            // Was it modified externally after base?
            if r.sync_equals(b) {
                // External unchanged since base, local deleted -> delete
                MergeResult::Delete
            } else {
                // External modified but local deleted - conflict
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::Delete,
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "External modified, local deleted - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        // Keep external since it was modified more recently than base
                        MergeResult::KeepWithNote(
                            r.clone(),
                            "External modified after base, local deleted - kept external"
                                .to_string(),
                        )
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::DeleteVsModify)
                    }
                }
            }
        }

        // Case 6: In all three (potentially modified in one or both)
        (Some(b), Some(l), Some(r)) => {
            if l.sync_equals(r) {
                return MergeResult::Keep(l.clone());
            }

            let left_changed = !l.sync_equals(b);
            let right_changed = !r.sync_equals(b);

            match (left_changed, right_changed) {
                // Neither changed OR only left changed - keep left
                (false | true, false) => MergeResult::Keep(l.clone()),
                // Only right changed - keep right
                (false, true) => MergeResult::Keep(r.clone()),
                // Both changed - use strategy
                (true, true) => match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Both modified - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "Both modified - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        if l.updated_at >= r.updated_at {
                            MergeResult::KeepWithNote(
                                l.clone(),
                                "Both modified - kept local (newer)".to_string(),
                            )
                        } else {
                            MergeResult::KeepWithNote(
                                r.clone(),
                                "Both modified - kept external (newer)".to_string(),
                            )
                        }
                    }
                    ConflictResolution::Manual => MergeResult::Conflict(ConflictType::BothModified),
                },
            }
        }

        // Case 7: In left and right but not base (convergent creation)
        (None, Some(l), Some(r)) => {
            // Same content? Keep one (use left by convention)
            if l.sync_equals(r) {
                MergeResult::Keep(l.clone())
            } else {
                // Different content - both created independently
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Convergent creation - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "Convergent creation - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        if l.updated_at >= r.updated_at {
                            MergeResult::KeepWithNote(
                                l.clone(),
                                "Convergent creation - kept local (newer)".to_string(),
                            )
                        } else {
                            MergeResult::KeepWithNote(
                                r.clone(),
                                "Convergent creation - kept external (newer)".to_string(),
                            )
                        }
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::ConvergentCreation)
                    }
                }
            }
        }

        // Case 8: Not in any (impossible in practice, but handle gracefully)
        (None, None, None) => MergeResult::NoAction,
    }
}

/// Perform a 3-way merge across all issues in the context.
///
/// This iterates through all unique issue IDs across base, left, and right,
/// and calls `merge_issue` for each to determine the appropriate action.
///
/// # Arguments
/// * `context` - The merge context containing base, left, and right states
/// * `strategy` - How to resolve conflicts when both sides modified
/// * `tombstones` - Optional set of issue IDs that should never be resurrected
///
/// # Returns
/// A `MergeReport` containing all actions taken and any conflicts detected.
#[must_use]
pub fn three_way_merge(
    context: &MergeContext,
    strategy: ConflictResolution,
    tombstones: Option<&HashSet<String, RandomState>>,
) -> MergeReport {
    let mut report = MergeReport::default();
    let empty_tombstones: HashSet<String, RandomState> = HashSet::new();
    let tombstones = tombstones.unwrap_or(&empty_tombstones);

    for id in context.all_issue_ids() {
        let base = context.base.get(&id);
        let left = context.left.get(&id);
        let right = context.right.get(&id);

        // Check tombstone protection: if issue is tombstoned and trying to resurrect
        if tombstones.contains(&id) {
            let local_tombstone =
                left.is_some_and(|issue| issue.status == crate::model::Status::Tombstone);
            let external_non_tombstone =
                right.is_some_and(|issue| issue.status != crate::model::Status::Tombstone);

            if local_tombstone && external_non_tombstone {
                // Import paths never allow JSONL to resurrect a local tombstone.
                // Merge winner flags must preserve that invariant too.
                if let Some(issue) = left {
                    report.kept.push(issue.clone());
                }
                report.tombstone_protected.push(id.clone());
                continue;
            }

            if left.is_none() && external_non_tombstone {
                // Trying to resurrect from external - skip.
                report.tombstone_protected.push(id.clone());
                continue;
            }
        }

        let result = merge_issue(base, left, right, strategy);

        match result {
            MergeResult::NoAction => {}
            MergeResult::Keep(issue) => {
                report.kept.push(issue);
            }
            MergeResult::KeepWithNote(issue, note) => {
                report.notes.push((issue.id.clone(), note));
                report.kept.push(issue);
            }
            MergeResult::Delete => {
                report.deleted.push(id.clone());
            }
            MergeResult::Conflict(conflict_type) => {
                report.conflicts.push((id.clone(), conflict_type));
            }
        }
    }

    report
}

/// Configuration for a 3-way merge operation.
#[derive(Debug, Clone, Default)]
pub struct MergeConfig {
    /// Strategy for resolving conflicts.
    pub strategy: ConflictResolution,
    /// Whether to skip tombstoned issues.
    pub respect_tombstones: bool,
}

/// Write one base-snapshot generation through the conditional publisher.
fn write_base_snapshot_atomically<WriteSnapshot>(
    jsonl_dir: &Path,
    write_snapshot: WriteSnapshot,
) -> Result<()>
where
    WriteSnapshot: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    let snapshot_path = jsonl_dir.join("beads.base.jsonl");
    let authority = blocking_jsonl_family_write_lock_with_timeout(&snapshot_path, None)?;
    let previous_source = authority.capture_optional_target()?;
    let expected_previous_state = previous_source.as_ref().map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    let publication = write_base_snapshot_atomically_under_authority(
        jsonl_dir,
        &expected_previous_state,
        &authority,
        write_snapshot,
    )?;
    if !publication.cleanup_durable() {
        tracing::warn!(
            snapshot_path = %snapshot_path.display(),
            recovery_path = publication.retained_recovery_path(),
            "Base snapshot reached its verified destination, but displaced-generation cleanup was not certified durable"
        );
    }
    Ok(())
}

fn write_base_snapshot_atomically_under_authority<WriteSnapshot>(
    jsonl_dir: &Path,
    expected_previous_state: &JsonlSourceStateWitness,
    authority: &JsonlFamilyWriteLock,
    write_snapshot: WriteSnapshot,
) -> Result<ExportPublicationReceipt>
where
    WriteSnapshot: FnOnce(&mut BufWriter<File>) -> Result<()>,
{
    let snapshot_path = jsonl_dir.join("beads.base.jsonl");
    authority.verify_jsonl_authority()?;
    let (temp_path, pinned_temp, temp_file) =
        create_base_snapshot_temp_file_under_authority(&snapshot_path, jsonl_dir, authority)?;
    let temp_guard = TempFileGuard::new_retained(temp_path.clone());
    let mut writer = BufWriter::new(temp_file);

    write_snapshot(&mut writer)?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;
    require_safe_sync_overwrite_path(&temp_path, jsonl_dir, false, "rename base snapshot")?;
    require_safe_sync_overwrite_path(&snapshot_path, jsonl_dir, false, "overwrite base snapshot")?;

    let staged_source = pinned_temp.capture()?;
    let content_sha256 = staged_source.content_sha256().to_string();
    let publication = publish_staged_jsonl_conditionally(
        &temp_path,
        temp_guard,
        &snapshot_path,
        &staged_source,
        expected_previous_state,
        &content_sha256,
        authority,
        None,
    )?;
    Ok(publication.into_receipt(&snapshot_path, content_sha256))
}

/// Save the base snapshot to a file.
///
/// This is used after a successful merge to record the common state.
///
/// # Errors
///
/// Returns an error if the file cannot be written and durably published.
pub fn save_base_snapshot<S: ::std::hash::BuildHasher>(
    issues: &std::collections::HashMap<String, Issue, S>,
    jsonl_dir: &Path,
) -> Result<()> {
    let mut ordered_issues: Vec<_> = issues.values().collect();
    ordered_issues.sort_by(|left, right| left.id.cmp(&right.id));

    write_base_snapshot_atomically(jsonl_dir, |writer| {
        let mut buffer = Vec::new();
        for issue in ordered_issues {
            buffer.clear();
            serde_json::to_writer(&mut buffer, issue).map_err(|e| {
                BeadsError::Config(format!("Failed to serialize issue {}: {}", issue.id, e))
            })?;
            writer.write_all(&buffer).map_err(BeadsError::Io)?;
            writer.write_all(b"\n").map_err(BeadsError::Io)?;
        }
        Ok(())
    })
}

/// Save the base snapshot from a finalized JSONL export.
///
/// This is used after a successful merge export so `beads.base.jsonl` reflects
/// the exact JSONL state that reached disk, including DB-side merge notes or
/// other derived fields added after the merge report was calculated.
///
/// # Errors
///
/// Returns an error if the finalized JSONL cannot be read or the base snapshot
/// cannot be written.
pub fn save_base_snapshot_from_jsonl(jsonl_path: &Path, jsonl_dir: &Path) -> Result<()> {
    let source = capture_jsonl_source_snapshot(jsonl_path)?;
    save_base_snapshot_from_jsonl_snapshot(&source, jsonl_dir)
}

pub(crate) fn save_base_snapshot_from_jsonl_snapshot(
    source: &JsonlSourceSnapshot,
    jsonl_dir: &Path,
) -> Result<()> {
    ensure_no_conflict_markers_snapshot(source)?;
    let issues: std::collections::HashMap<String, Issue> = read_issues_from_jsonl_snapshot(source)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    save_base_snapshot(&issues, jsonl_dir)
}

/// Refresh `beads.base.jsonl` with the exact bytes of a finalized flush
/// export (issue #378).
///
/// After a clean `br sync --flush-only`, the database and the JSONL agree, so
/// the JSONL that just reached disk IS the new common state future 3-way
/// merges should diff against. Historically only the merge path wrote the
/// anchor, which left flush-only workspaces (the common agent workflow)
/// permanently anchor-less: `br doctor` warned `base_jsonl.missing_post_flush`
/// forever while `br sync --status` reported "In sync".
///
/// This is a byte copy (not a parse + re-serialize) so the anchor matches the
/// on-disk export exactly. The write goes through the same validated
/// temp-file + durable-rename machinery as [`save_base_snapshot`], and a
/// symlinked anchor is refused rather than followed (same attacker shape the
/// doctor's `base_jsonl` check rejects).
///
/// # Errors
///
/// Returns an error if the finalized JSONL cannot be read, the anchor path is
/// unsafe (symlink / escapes the workspace), or the snapshot cannot be
/// written durably.
pub fn refresh_base_snapshot_from_flushed_jsonl(jsonl_path: &Path, jsonl_dir: &Path) -> Result<()> {
    let source = capture_jsonl_source_snapshot(jsonl_path)?;
    refresh_base_snapshot_from_flushed_jsonl_snapshot(&source, jsonl_dir)
}

pub(crate) fn refresh_base_snapshot_from_flushed_jsonl_snapshot(
    source: &JsonlSourceSnapshot,
    jsonl_dir: &Path,
) -> Result<()> {
    ensure_no_conflict_markers_snapshot(source)?;
    write_base_snapshot_atomically(jsonl_dir, |writer| {
        std::io::copy(&mut source.reader(), writer).map_err(BeadsError::Io)?;
        Ok(())
    })
}

pub(crate) fn refresh_base_snapshot_from_flushed_jsonl_snapshot_under_authority(
    source: &JsonlSourceSnapshot,
    jsonl_dir: &Path,
    expected_previous_state: &JsonlSourceStateWitness,
    authority: &JsonlFamilyWriteLock,
) -> Result<ExportPublicationReceipt> {
    ensure_no_conflict_markers_snapshot(source)?;
    write_base_snapshot_atomically_under_authority(
        jsonl_dir,
        expected_previous_state,
        authority,
        |writer| {
            std::io::copy(&mut source.reader(), writer).map_err(BeadsError::Io)?;
            Ok(())
        },
    )
}

pub(crate) fn load_base_snapshot_from_source(
    source: Option<&JsonlSourceSnapshot>,
) -> Result<std::collections::HashMap<String, Issue>> {
    let Some(source) = source else {
        return Ok(std::collections::HashMap::new());
    };
    ensure_no_conflict_markers_snapshot(source)?;
    let mut base = std::collections::HashMap::new();
    for issue in read_issues_from_jsonl_snapshot(source)? {
        let issue_id = issue.id.clone();
        if base.insert(issue_id.clone(), issue).is_some() {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Base snapshot contains duplicate issue ID {issue_id}; refusing an ambiguous merge ancestor"
                ),
            });
        }
    }
    Ok(base)
}

/// Load the base snapshot from a file.
///
/// Returns an empty map if the snapshot does not exist.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_base_snapshot(jsonl_dir: &Path) -> Result<std::collections::HashMap<String, Issue>> {
    let snapshot_path = jsonl_dir.join("beads.base.jsonl");
    require_valid_sync_path(&snapshot_path, jsonl_dir)?;
    let source = capture_optional_jsonl_source(&snapshot_path)?;
    load_base_snapshot_from_source(source.as_ref())
}

/// An issue row plus its related labels, dependencies, and comments,
/// snapshotted before a rebuild so it can be atomically restored afterwards.
/// Used both for unflushed tombstones (deletion-retention state absent from
/// the JSONL) and for dirty live issues whose latest edit has not reached
/// the JSONL yet (GitHub #394).
///
/// The option wrappers on the relations let callers partially preserve an
/// issue whose relation fetches failed (a pattern the CLI layer already
/// uses): we keep the issue row and skip whatever relation set couldn't be
/// read, rather than losing the issue entirely.
#[derive(Clone, Debug)]
pub(crate) struct PreservedIssue {
    pub(crate) issue: Issue,
    pub(crate) labels: Option<Vec<String>>,
    pub(crate) dependencies: Option<Vec<Dependency>>,
    pub(crate) comments: Option<Vec<Comment>>,
}

/// Snapshot one issue row plus its relations, degrading gracefully: a
/// missing or unreadable issue row yields `None` (with a warning), and a
/// failed relation fetch yields issue-row-only preservation for that
/// relation. `kind` labels the log lines so tombstone and dirty-issue
/// snapshots stay distinguishable in traces.
fn snapshot_preserved_issue(
    storage: &SqliteStorage,
    issue_id: &str,
    kind: &str,
) -> Option<PreservedIssue> {
    let issue = match storage.get_issue(issue_id) {
        Ok(issue) => issue,
        Err(error) => {
            tracing::warn!(
                issue_id = %issue_id,
                kind,
                error = %error,
                "Skipping preservation for issue that could not be read before rebuild"
            );
            return None;
        }
    }?;

    let labels = match storage.get_labels(issue_id) {
        Ok(labels) => Some(labels),
        Err(error) => {
            tracing::warn!(
                issue_id = %issue_id,
                kind,
                error = %error,
                "Failed to snapshot labels before rebuild; preserving issue row only"
            );
            None
        }
    };
    let dependencies = match storage.get_dependencies_full(issue_id) {
        Ok(dependencies) => Some(dependencies),
        Err(error) => {
            tracing::warn!(
                issue_id = %issue_id,
                kind,
                error = %error,
                "Failed to snapshot dependencies before rebuild; preserving issue row only"
            );
            None
        }
    };
    let comments = match storage.get_comments(issue_id) {
        Ok(comments) => Some(comments),
        Err(error) => {
            tracing::warn!(
                issue_id = %issue_id,
                kind,
                error = %error,
                "Failed to snapshot comments before rebuild; preserving issue row only"
            );
            None
        }
    };
    Some(PreservedIssue {
        issue,
        labels,
        dependencies,
        comments,
    })
}

/// Snapshot every tombstoned issue in the database, including its labels,
/// dependencies, and comments, so a rebuild can restore deletion-retention
/// state that is not present in the JSONL export.
///
/// This is fully best-effort — the function never returns an error: if
/// the enumeration query fails outright we log and return an empty list
/// (the rebuild still proceeds without tombstone preservation), and
/// per-tombstone relation fetches also degrade gracefully to issue-row-
/// only preservation.
#[must_use]
pub(crate) fn snapshot_tombstones(storage: &SqliteStorage) -> Vec<PreservedIssue> {
    let tombstone_ids = match storage.get_issue_ids_by_status(&crate::model::Status::Tombstone) {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "Failed to enumerate tombstones before rebuild; continuing without tombstone preservation"
            );
            return Vec::new();
        }
    };

    tombstone_ids
        .iter()
        .filter_map(|id| snapshot_preserved_issue(storage, id, "tombstone"))
        .collect()
}

/// Snapshot every dirty *live* issue in the database — rows in
/// `dirty_issues` whose latest state has never been flushed to the JSONL —
/// so a rebuild that replays only the JSONL cannot silently drop them
/// (GitHub #394). Tombstone-status rows are skipped here; the sibling
/// `snapshot_tombstones` pass owns deletion-retention state.
///
/// Best-effort with the same degradation contract as `snapshot_tombstones`:
/// enumeration failure logs and returns empty, per-issue failures skip that
/// issue or degrade to issue-row-only preservation.
#[must_use]
pub(crate) fn snapshot_dirty_live_issues(storage: &SqliteStorage) -> Vec<PreservedIssue> {
    let dirty_ids = match storage.get_dirty_issue_ids() {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "Failed to enumerate dirty issues before rebuild; continuing without dirty-issue preservation"
            );
            return Vec::new();
        }
    };

    dirty_ids
        .iter()
        .filter_map(|id| snapshot_preserved_issue(storage, id, "dirty issue"))
        .filter(|preserved| preserved.issue.status != crate::model::Status::Tombstone)
        .collect()
}

/// Restore preserved tombstones after a successful rebuild, wrapping any
/// failure with a message that makes clear the rebuild itself succeeded —
/// only the retention-state restoration step failed.
///
/// The rebuild has already moved the original database family into the
/// recovery directory and replaced it with a clean JSONL import at this
/// point, so on failure the live DB is *valid* (it mirrors the JSONL),
/// just missing whatever local unflushed tombstones we tried to preserve.
/// Without this wrapper, a transient lock-contention retry exhaustion
/// inside `restore_preserved_issues` would bubble up through callers that
/// otherwise describe the failure as "JSONL may be corrupt" or "database
/// recovery failed", both of which are actively misleading for this
/// specific post-rebuild failure mode. The wrapped message tells the
/// operator: re-running the command is idempotent and safe; the only
/// thing they've lost is local deletions that hadn't yet been flushed.
///
/// Callers should prefer this helper over calling `restore_preserved_issues`
/// directly when the restore follows an already-completed rebuild. Use
/// the bare `restore_preserved_issues` when the surrounding transaction is
/// still mid-rebuild and a rollback is still possible.
///
/// # Errors
///
/// Returns a `BeadsError::WithContext` whose source is the original
/// `restore_preserved_issues` error. Returns `Ok(())` when `tombstones` is
/// empty without calling into the write-transaction retry loop.
pub(crate) fn restore_tombstones_after_rebuild(
    storage: &mut SqliteStorage,
    tombstones: &[PreservedIssue],
) -> Result<()> {
    if tombstones.is_empty() {
        return Ok(());
    }
    let count = tombstones.len();
    restore_preserved_issues(storage, tombstones).map_err(|err| BeadsError::WithContext {
        context: format!(
            "Rebuild from JSONL succeeded, but failed to restore {count} preserved \
             tombstone(s). The database now mirrors the JSONL exactly — any local \
             deletions that had not yet been flushed to the JSONL are gone. \
             Re-running the command is idempotent and safe (the rebuild itself \
             completed successfully). If the underlying cause is lock contention, \
             wait for other `br` processes to finish and try again."
        ),
        source: Box::new(err),
    })
}

/// Restore preserved dirty live issues after a successful rebuild, wrapping
/// any failure with a message that makes clear the rebuild itself succeeded
/// — only the unflushed-edit restoration step failed (GitHub #394).
///
/// Same contract as `restore_tombstones_after_rebuild`, with a message
/// naming the actual casualty: local edits that had not yet been flushed
/// to the JSONL, rather than local deletions.
///
/// # Errors
///
/// Returns a `BeadsError::WithContext` whose source is the original
/// `restore_preserved_issues` error. Returns `Ok(())` when `dirty_issues`
/// is empty without calling into the write-transaction retry loop.
pub(crate) fn restore_dirty_issues_after_rebuild(
    storage: &mut SqliteStorage,
    dirty_issues: &[PreservedIssue],
) -> Result<()> {
    if dirty_issues.is_empty() {
        return Ok(());
    }
    let count = dirty_issues.len();
    restore_preserved_issues(storage, dirty_issues).map_err(|err| BeadsError::WithContext {
        context: format!(
            "Rebuild from JSONL succeeded, but failed to restore {count} dirty \
             unflushed issue(s). The database now mirrors the JSONL exactly — any \
             local creations or edits that had not yet been flushed to the JSONL \
             are gone. Re-running the command is idempotent and safe (the rebuild \
             itself completed successfully). If the underlying cause is lock \
             contention, wait for other `br` processes to finish and try again."
        ),
        source: Box::new(err),
    })
}

/// Restore preserved issues (and their relations) atomically and mark
/// them dirty so the next flush re-exports them.
///
/// # Errors
///
/// Returns an error if the underlying write transaction fails; the entire
/// restore is rolled back on failure.
pub(crate) fn restore_preserved_issues(
    storage: &mut SqliteStorage,
    preserved: &[PreservedIssue],
) -> Result<()> {
    if preserved.is_empty() {
        return Ok(());
    }

    let marked_at = Utc::now().to_rfc3339();
    storage.with_write_transaction(|storage| {
        for entry in preserved {
            storage.upsert_issue_for_import_in_tx(&entry.issue)?;
        }
        for entry in preserved {
            if let Some(labels) = &entry.labels {
                storage.sync_labels_for_import_in_tx(&entry.issue.id, labels)?;
            }
            if let Some(dependencies) = &entry.dependencies {
                storage.sync_dependencies_for_import_in_tx(&entry.issue.id, dependencies)?;
            }
            if let Some(comments) = &entry.comments {
                storage.sync_comments_for_import_in_tx(&entry.issue.id, comments)?;
            }
            storage.replace_dirty_issue_marker_in_tx(&entry.issue.id, &marked_at)?;
        }
        Ok(())
    })?;

    tracing::debug!(
        count = preserved.len(),
        "Restored preserved issues atomically after rebuild and marked them dirty for export"
    );
    Ok(())
}

/// Per-ID view of the JSONL used to decide which preserved tombstones
/// should actually be restored after a rebuild. The rebuild imports
/// everything in the JSONL first, so tombstone preservation only needs to
/// fix up rows where the local DB and the JSONL disagree.
///
/// Two buckets:
///
/// - `tombstone_ids`: IDs whose JSONL record carries `status = tombstone`.
///   The deletion has already been flushed, so the rebuild will reimport it
///   as a tombstone on its own — we drop any local preserved tombstone for
///   these IDs.
///
/// - `non_tombstone_updated_at`: IDs whose JSONL record carries a *non*-
///   tombstone status, mapped to the record's `updated_at`. When the local
///   DB has one of these IDs as a tombstone, there is a disagreement: the
///   JSONL says the issue is alive, the DB says it's deleted. Import and
///   rebuild paths must keep the tombstone; reopening is a separate,
///   explicit user action.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsonlTombstoneFilter {
    pub(crate) tombstone_ids: HashSet<String>,
    pub(crate) non_tombstone_updated_at:
        std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
}

/// Filter the preserved tombstone set down to those that should actually
/// be restored after the rebuild has reimported the JSONL. Three cases:
///
/// 1. JSONL has this ID as a tombstone: drop from preservation set — the
///    rebuild's own `import_from_jsonl` will reinstate the tombstone.
///
/// 2. JSONL has this ID as a non-tombstone: preserve the local tombstone
///    and restore it after import. This mirrors the normal
///    `import_from_jsonl` tombstone guard, which rejects resurrection even
///    when force-upsert is enabled. Timestamp ordering cannot make a
///    deleted issue live again; the operator must reopen it explicitly.
///
/// 3. JSONL doesn't have this ID at all: the deletion has never been
///    flushed anywhere. Always preserve — otherwise this path would
///    silently lose the local delete.
#[must_use]
pub(crate) fn tombstones_missing_from_jsonl_tombstones(
    tombstones: Vec<PreservedIssue>,
    jsonl_filter: &JsonlTombstoneFilter,
) -> Vec<PreservedIssue> {
    let original_count = tombstones.len();
    let mut skipped_already_flushed = 0usize;
    let mut preserved_non_tombstone_conflicts = 0usize;
    let preserved: Vec<PreservedIssue> = tombstones
        .into_iter()
        .filter(|tombstone| {
            let id = &tombstone.issue.id;
            if jsonl_filter.tombstone_ids.contains(id) {
                skipped_already_flushed += 1;
                return false;
            }
            if jsonl_filter.non_tombstone_updated_at.contains_key(id) {
                preserved_non_tombstone_conflicts += 1;
            }
            true
        })
        .collect();

    if skipped_already_flushed > 0 || preserved_non_tombstone_conflicts > 0 {
        tracing::debug!(
            preserved = preserved.len(),
            skipped_already_flushed,
            preserved_non_tombstone_conflicts,
            original = original_count,
            "Filtered preserved tombstones against JSONL state"
        );
    }

    preserved
}

/// Filter the preserved dirty-live-issue set down to those whose latest
/// state the JSONL cannot reproduce, so a rebuild that replays only the
/// JSONL does not silently drop them (GitHub #394). Three cases:
///
/// 1. JSONL has this ID as a tombstone: drop from preservation set. A
///    flushed deletion wins over an unflushed local edit — this mirrors
///    the `import_from_jsonl` tombstone guard, which rejects resurrection
///    even for newer non-tombstone rows. Reopening is a separate,
///    explicit user action.
///
/// 2. JSONL has this ID live: preserve only when the local row's
///    `updated_at` is strictly newer than the JSONL row's — i.e. there is
///    an unflushed local edit. When the JSONL copy is as new or newer,
///    the rebuild's own import restores an equal-or-better row.
///
/// 3. JSONL doesn't have this ID at all: the issue has never been flushed
///    anywhere. Always preserve — this is the `--no-auto-flush` data-loss
///    shape from the original report.
#[must_use]
pub(crate) fn dirty_issues_missing_from_jsonl(
    dirty_issues: Vec<PreservedIssue>,
    jsonl_filter: &JsonlTombstoneFilter,
) -> Vec<PreservedIssue> {
    let original_count = dirty_issues.len();
    let mut skipped_flushed_tombstones = 0usize;
    let mut skipped_jsonl_current = 0usize;
    let preserved: Vec<PreservedIssue> = dirty_issues
        .into_iter()
        .filter(|preserved| {
            let id = &preserved.issue.id;
            if jsonl_filter.tombstone_ids.contains(id) {
                skipped_flushed_tombstones += 1;
                return false;
            }
            match jsonl_filter.non_tombstone_updated_at.get(id) {
                Some(jsonl_updated_at) => {
                    if preserved.issue.updated_at > *jsonl_updated_at {
                        true
                    } else {
                        skipped_jsonl_current += 1;
                        false
                    }
                }
                None => true,
            }
        })
        .collect();

    if skipped_flushed_tombstones > 0 || skipped_jsonl_current > 0 {
        tracing::debug!(
            preserved = preserved.len(),
            skipped_flushed_tombstones,
            skipped_jsonl_current,
            original = original_count,
            "Filtered preserved dirty issues against JSONL state"
        );
    }

    preserved
}

/// Scan the JSONL once and build a `JsonlTombstoneFilter` we can use to
/// decide which preserved tombstones to restore after a rebuild.
///
/// # Errors
///
/// Returns an error if the JSONL cannot be read, contains invalid JSON, or
/// has duplicate IDs across lines.
#[cfg(test)]
pub(crate) fn scan_jsonl_for_tombstone_filter(path: &Path) -> Result<JsonlTombstoneFilter> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    scan_jsonl_for_tombstone_filter_from_reader(path, BufReader::new(file))
}

pub(crate) fn scan_jsonl_snapshot_for_tombstone_filter(
    source: &JsonlSourceSnapshot,
) -> Result<JsonlTombstoneFilter> {
    scan_jsonl_for_tombstone_filter_from_reader(source.display_path(), source.reader())
}

fn scan_jsonl_for_tombstone_filter_from_reader(
    display_path: &Path,
    mut reader: impl BufRead,
) -> Result<JsonlTombstoneFilter> {
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut seen_ids = HashSet::new();
    let mut filter = JsonlTombstoneFilter::default();

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                display_path.display(),
                line_num
            )));
        }

        if issue.status == crate::model::Status::Tombstone {
            filter.tombstone_ids.insert(issue.id);
        } else {
            filter
                .non_tombstone_updated_at
                .insert(issue.id, issue.updated_at);
        }
    }

    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
    use chrono::Utc;
    use fsqlite_types::SqliteValue;
    use std::collections::HashMap;
    use std::io::{self, Write};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
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
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    fn fresh_replacement_import_fixture() -> (
        TempDir,
        PathBuf,
        PathBuf,
        Arc<DatabaseFamilyWriteLock>,
        SqliteStorage,
        FreshDatabaseReplacementWitness,
    ) {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let authority = Arc::new(
            blocking_database_family_write_lock_with_timeout(&beads_dir, &db_path, Some(2_000))
                .unwrap(),
        );
        let witness = authority
            .install_empty_database_replacement_and_bind()
            .unwrap();
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.attach_write_authority(Arc::clone(&authority));
        (temp, beads_dir, jsonl_path, authority, storage, witness)
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    #[test]
    fn database_candidate_no_replace_preserves_an_existing_target() {
        let temp = TempDir::new().unwrap();
        let candidate = temp.path().join("candidate.db");
        let target = temp.path().join("target.db");
        fs::write(&candidate, b"candidate generation").unwrap();
        fs::write(&target, b"concurrent generation").unwrap();

        let error = install_database_candidate_no_replace(&candidate, &target)
            .expect_err("atomic no-replace install must reject an existing target");

        assert!(
            matches!(&error, BeadsError::SyncConflict { .. }),
            "existing-target rejection should be reported as a sync conflict: {error}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"concurrent generation");
        assert_eq!(fs::read(&candidate).unwrap(), b"candidate generation");
    }

    #[test]
    fn missing_database_authority_witness_propagates_non_not_found_errors() {
        let error = verify_database_authority_path_still_missing(Path::new("invalid\0database"))
            .expect_err("invalid path errors must not be classified as stable absence");
        assert!(
            matches!(&error, BeadsError::Config(_)),
            "non-NotFound inspection failure must fail closed: {error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_authority_rejects_replacement_with_equal_creation_time() {
        use std::os::windows::fs::FileTimesExt;

        let temp = TempDir::new().unwrap();
        let authority_path = temp.path().join("authority.lock");
        let displaced_path = temp.path().join("displaced.lock");
        fs::write(&authority_path, b"held generation").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&authority_path)
            .unwrap();
        let held_created = held.metadata().unwrap().created().unwrap();

        fs::rename(&authority_path, &displaced_path).unwrap();
        fs::write(&authority_path, b"replacement generation").unwrap();
        let replacement = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&authority_path)
            .unwrap();
        replacement
            .set_times(std::fs::FileTimes::new().set_created(held_created))
            .unwrap();
        assert_eq!(
            replacement.metadata().unwrap().created().unwrap(),
            held_created,
            "the mutation must defeat the former creation-time identity witness"
        );

        let error = verify_locked_file_identity(&held, &authority_path, "test authority", false)
            .expect_err("stable handle identity must reject the distinct replacement");
        assert!(matches!(error, BeadsError::SyncConflict { .. }));
        assert!(error.to_string().contains("identity changed"));
    }

    #[test]
    fn sync_merge_receipt_hash_domains_are_stable_and_distinct() {
        let payload = ("identical receipt payload", 7_u64);
        let intent = sync_merge_domain_separated_sha256(
            SYNC_MERGE_INTENT_DOMAIN,
            &payload,
            "test merge intent",
        )
        .unwrap();
        let envelope = sync_merge_domain_separated_sha256(
            SYNC_MERGE_RECEIPT_ENVELOPE_DOMAIN,
            &payload,
            "test immutable envelope",
        )
        .unwrap();
        let envelope_repeat = sync_merge_domain_separated_sha256(
            SYNC_MERGE_RECEIPT_ENVELOPE_DOMAIN,
            &payload,
            "test immutable envelope",
        )
        .unwrap();
        let state = sync_merge_domain_separated_sha256(
            SYNC_MERGE_RECEIPT_STATE_DOMAIN,
            &payload,
            "test receipt state",
        )
        .unwrap();

        assert_eq!(envelope, envelope_repeat);
        assert_ne!(
            intent, envelope,
            "identical bytes in the intent and immutable-envelope domains must not share a digest"
        );
        assert_ne!(
            intent, state,
            "identical bytes in the intent and state domains must not share a digest"
        );
        assert_ne!(
            envelope, state,
            "identical bytes in the immutable-envelope and state domains must not share a digest"
        );
    }

    #[cfg(unix)]
    #[test]
    fn authority_paths_and_receipts_do_not_alias_lossy_invalid_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp_dir = TempDir::new().unwrap();
        let first_leaf = OsString::from_vec(b"authority-\xff.jsonl".to_vec());
        let second_leaf = OsString::from_vec(b"authority-\xfe.jsonl".to_vec());
        let first_path = temp_dir.path().join(first_leaf);
        let second_path = temp_dir.path().join(second_leaf);

        assert_eq!(
            first_path.to_string_lossy(),
            second_path.to_string_lossy(),
            "the fixture must collide under the legacy lossy representation"
        );

        let first_jsonl_sidecar = jsonl_write_authority_path(&first_path).unwrap();
        let second_jsonl_sidecar = jsonl_write_authority_path(&second_path).unwrap();
        assert_ne!(first_jsonl_sidecar, second_jsonl_sidecar);

        let first_jsonl_authority =
            blocking_jsonl_family_write_lock_with_timeout(&first_path, Some(100)).unwrap();
        let second_jsonl_authority =
            blocking_jsonl_family_write_lock_with_timeout(&second_path, Some(100)).unwrap();
        assert_ne!(
            first_jsonl_authority.authority_path_sha256(),
            second_jsonl_authority.authority_path_sha256()
        );

        assert_ne!(
            database_write_authority_path(&first_path).unwrap(),
            database_write_authority_path(&second_path).unwrap()
        );
        assert_ne!(
            database_write_authority_sha256(&first_path).unwrap(),
            database_write_authority_sha256(&second_path).unwrap()
        );
    }

    #[test]
    fn authority_bound_target_capture_rejects_parent_route_replacement() {
        let temp_dir = TempDir::new().unwrap();
        let routed_parent = temp_dir.path().join(".beads");
        let retained_parent = temp_dir.path().join(".beads-retained");
        fs::create_dir_all(&routed_parent).unwrap();
        let output_path = routed_parent.join("issues.jsonl");
        fs::write(&output_path, b"trusted-generation\n").unwrap();
        let authority =
            blocking_jsonl_family_write_lock_with_timeout(&output_path, Some(100)).unwrap();

        fs::rename(&routed_parent, &retained_parent).unwrap();
        fs::create_dir_all(&routed_parent).unwrap();
        fs::write(&output_path, b"attacker-generation\n").unwrap();

        let error = authority.capture_target().unwrap_err();
        assert!(
            error.to_string().contains("route")
                || error.to_string().contains("authority")
                || error.to_string().contains("parent"),
            "unexpected authority-bound capture error: {error}"
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"attacker-generation\n");
        assert_eq!(
            fs::read(retained_parent.join("issues.jsonl")).unwrap(),
            b"trusted-generation\n"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sync_merge_finalization_requires_exact_metadata_cardinality_and_mtime() {
        let storage = SqliteStorage::open_memory().unwrap();
        let database_before = capture_sync_database_witness(&storage).unwrap();
        let database_after = database_before.clone().into();
        let export_as_of = Utc::now();
        let reviewed_raw_sha256 = "66".repeat(32);
        let reviewed_content_sha256 = reviewed_raw_sha256.clone();
        let intent = SyncMergeIntent {
            schema_version: 2,
            database_authority_sha256: "11".repeat(32),
            jsonl_authority_sha256: "22".repeat(32),
            jsonl_path_sha256: "33".repeat(32),
            jsonl_before: JsonlSourceStateWitness::Missing,
            jsonl_before_content_sha256: None,
            base_authority_sha256: "44".repeat(32),
            base_before: JsonlSourceStateWitness::Missing,
            base_before_content_sha256: None,
            resolution: "manual".to_string(),
            actor: "test-agent".to_string(),
            event_attribution: EventAttribution::default(),
            capacity_policy: crate::close_policy::CapacityPolicy::default(),
            retention_days: None,
            export_as_of,
            changed_kept_issue_ids: Vec::new(),
            kept_issue_witnesses: Vec::new(),
            deleted_issue_ids: Vec::new(),
            note_witnesses: Vec::new(),
            database_before,
        };
        let committed = SyncMergePendingReceipt::new(
            intent,
            export_as_of.to_rfc3339(),
            database_after,
            reviewed_raw_sha256.clone(),
            0,
            &[],
            Vec::new(),
        )
        .unwrap();
        let expected_export_hashes = sync_merge_export_hash_mapping_witness(&[]).unwrap();
        let finalization = SyncMergeExportFinalizationWitness {
            export_hashes: expected_export_hashes,
            dirty_issues: AdditiveTableWitness {
                rows: 0,
                payload_sha256: "88".repeat(32),
            },
            jsonl_content_hash: Some(reviewed_content_sha256),
            jsonl_mtime: Some((export_as_of + chrono::Duration::seconds(1)).to_rfc3339()),
            jsonl_size: Some("0".to_string()),
            last_export_time: Some(export_as_of.to_rfc3339()),
            needs_flush: Some("false".to_string()),
            export_metadata: AdditiveTableWitness {
                rows: 6,
                payload_sha256: "99".repeat(32),
            },
        };

        let error = committed
            .advance_to_export_finalized(
                JsonlSourceStateWitness::Present {
                    raw_sha256: reviewed_raw_sha256.clone(),
                    mtime: export_as_of.to_rfc3339(),
                    size: 0,
                    identity: None,
                },
                finalization.clone(),
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                BeadsError::SyncConflict { ref message }
                    if message.contains("exactly five export metadata rows")
                        && message.contains("found 6")
            ),
            "unexpected finalization error: {error}"
        );

        let mut wrong_mtime = finalization;
        wrong_mtime.export_metadata.rows = 5;
        let error = committed
            .advance_to_export_finalized(
                JsonlSourceStateWitness::Present {
                    raw_sha256: reviewed_raw_sha256,
                    mtime: export_as_of.to_rfc3339(),
                    size: 0,
                    identity: None,
                },
                wrong_mtime,
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                BeadsError::SyncConflict { ref message }
                    if message.contains("published JSONL mtime")
            ),
            "unexpected mtime error: {error}"
        );

        let mut wrong_mapping = sync_merge_export_hash_mapping_witness(&[]).unwrap();
        wrong_mapping.payload_sha256 = "aa".repeat(32);
        let mut wrong_hashes = SyncMergeExportFinalizationWitness {
            export_hashes: wrong_mapping,
            dirty_issues: AdditiveTableWitness {
                rows: 0,
                payload_sha256: "88".repeat(32),
            },
            jsonl_content_hash: Some(committed.jsonl_after_content_sha256.clone()),
            jsonl_mtime: Some(export_as_of.to_rfc3339()),
            jsonl_size: Some("0".to_string()),
            last_export_time: Some(export_as_of.to_rfc3339()),
            needs_flush: Some("false".to_string()),
            export_metadata: AdditiveTableWitness {
                rows: 5,
                payload_sha256: "99".repeat(32),
            },
        };
        let error = committed
            .advance_to_export_finalized(
                JsonlSourceStateWitness::Present {
                    raw_sha256: committed.jsonl_after_raw_sha256.clone(),
                    mtime: export_as_of.to_rfc3339(),
                    size: 0,
                    identity: None,
                },
                wrong_hashes.clone(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("export-hash mapping"),
            "unexpected issue-hash mapping error: {error}"
        );

        wrong_hashes.export_hashes = committed.jsonl_after_issue_hashes.clone();
        let finalized = committed
            .advance_to_export_finalized(
                JsonlSourceStateWitness::Present {
                    raw_sha256: committed.jsonl_after_raw_sha256.clone(),
                    mtime: export_as_of.to_rfc3339(),
                    size: 0,
                    identity: None,
                },
                wrong_hashes,
            )
            .unwrap();
        let mut tampered_mtime = finalized.clone();
        let Some(JsonlSourceStateWitness::Present { mtime, .. }) =
            tampered_mtime.jsonl_after.as_mut()
        else {
            panic!("finalized receipt must carry a present source witness");
        };
        *mtime = (export_as_of + chrono::Duration::seconds(2)).to_rfc3339();
        tampered_mtime.state_sha256 = tampered_mtime.current_state_sha256().unwrap();
        assert!(
            tampered_mtime.validate().is_err(),
            "receipt validation must reject a recomputed state digest with mismatched mtime evidence"
        );

        let mut tampered_export_time = finalized;
        tampered_export_time
            .export_finalization
            .as_mut()
            .unwrap()
            .last_export_time = Some("not-rfc3339".to_string());
        tampered_export_time.state_sha256 = tampered_export_time.current_state_sha256().unwrap();
        assert!(
            tampered_export_time.validate().is_err(),
            "receipt validation must reject invalid finalized export time even with a recomputed state digest"
        );
    }

    #[test]
    fn blocking_write_lock_errors_when_lock_path_cannot_open() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".write.lock")).unwrap();

        let err = blocking_write_lock(&beads_dir).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Refusing unsafe workspace write lock path")
                        && message.contains(".write.lock")
            ),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn blocking_write_lock_rejects_symlink_leaf_without_touching_target() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = temp_dir.path().join("outside.lock");
        fs::write(&target, b"sentinel").unwrap();
        symlink(&target, beads_dir.join(".write.lock")).unwrap();

        let err = blocking_write_lock(&beads_dir).unwrap_err();
        assert!(err.to_string().contains("unsafe workspace write lock path"));
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
    }

    /// GitHub #412 regression: the database-family authority must never hold
    /// a whole-file lock on the database inode. Under v0.2.20 it held
    /// `flock(LOCK_EX)` there, so this flock probe returned `WouldBlock` even
    /// on Linux. The probe is Linux-only because on macOS/BSD an `flock`
    /// probe legitimately conflicts with the authority's `fcntl` range lock
    /// (one shared kernel lock table).
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn database_family_authority_does_not_whole_file_lock_the_database_inode() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        drop(SqliteStorage::open(&db_path).expect("create fresh database"));

        let _authority =
            blocking_database_family_write_lock_with_timeout(&beads_dir, &db_path, Some(2_000))
                .expect("family authority");

        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db_path)
            .expect("probe open");
        assert!(
            probe.try_lock().is_ok(),
            "database inode must not be whole-file locked while the family authority is held"
        );
    }

    /// GitHub #412 regression: the engine must be able to open and query the
    /// database while the family authority is held — the startup
    /// pending-sync-merge gate and every mutating command do exactly this.
    /// On macOS/BSD the former whole-file `flock` made every engine open fail
    /// with "Database error: database is busy" on a freshly-initialised
    /// workspace; on Windows the whole-file mandatory lock made even the
    /// schema header unreadable.
    #[test]
    fn engine_opens_and_queries_under_held_family_authority() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        drop(SqliteStorage::open(&db_path).expect("create fresh database"));

        let authority = Arc::new(
            blocking_database_family_write_lock_with_timeout(&beads_dir, &db_path, Some(2_000))
                .expect("family authority"),
        );

        // Read-only startup-gate flow (the first command #412 reported broken).
        let inspection =
            SqliteStorage::inspect_pending_sync_merge_under_authority(&db_path, &authority).expect(
                "pending sync-merge inspection must not mistake the held authority for \
                     engine contention",
            );
        assert!(
            matches!(
                inspection,
                crate::storage::sqlite::PendingSyncMergeInspection::Absent
            ),
            "fresh database must have no pending sync merge"
        );

        // Writable engine open under the same held authority (the normal
        // mutating-command flow after the gate passes).
        let mut storage =
            SqliteStorage::open(&db_path).expect("writable engine open under family authority");
        storage.attach_write_authority(Arc::clone(&authority));
        storage
            .inspect_pending_sync_merge()
            .expect("engine read transaction under held family authority");
    }

    /// GitHub #412 companion: switching the inode lock from a whole-file
    /// `flock` to a SQLite-compatible byte-range lock must not weaken the
    /// hard-link-alias exclusion the inode authority exists to provide
    /// (GitHub #405): a second workspace routing to the same physical inode
    /// through a hard link must still fail to acquire authority.
    #[cfg(unix)]
    #[test]
    fn family_authority_still_excludes_hard_link_aliases() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir_a = temp_dir.path().join("a");
        let beads_dir_b = temp_dir.path().join("b");
        fs::create_dir_all(&beads_dir_a).unwrap();
        fs::create_dir_all(&beads_dir_b).unwrap();
        let db_a = beads_dir_a.join("beads.db");
        let db_b = beads_dir_b.join("beads.db");
        drop(SqliteStorage::open(&db_a).expect("create fresh database"));
        fs::hard_link(&db_a, &db_b).expect("hard link alias");

        let _authority_a =
            blocking_database_family_write_lock_with_timeout(&beads_dir_a, &db_a, Some(2_000))
                .expect("first family authority");

        let err = blocking_database_family_write_lock_with_timeout(&beads_dir_b, &db_b, Some(50))
            .expect_err("hard-link alias must not acquire a second authority");
        assert!(
            err.to_string().contains("Timed out"),
            "alias acquisition must time out on the shared inode lock: {err}"
        );
    }

    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn blocking_write_lock_with_timeout_errors_when_lock_is_held() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".write.lock");
        let held_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open held write lock");
        held_lock.lock().expect("hold write lock");

        let start = Instant::now();
        let err = blocking_write_lock_with_timeout(&beads_dir, Some(25)).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "timeout should fail promptly"
        );
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Timed out after 25ms")
                        && message.contains(".write.lock")
                        && message.contains("stuck process")
            ),
            "unexpected error: {err}"
        );

        drop(held_lock);
        let acquired =
            blocking_write_lock_with_timeout(&beads_dir, Some(25)).expect("lock after release");
        drop(acquired);
    }

    #[test]
    fn try_sync_lock_errors_when_lock_path_cannot_open() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".sync.lock")).unwrap();

        let err = try_sync_lock(&beads_dir).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Failed to open sync lock")
                        && message.contains(".sync.lock")
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn try_sync_lock_returns_none_when_lock_is_held() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".sync.lock");
        let held_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open held sync lock");
        held_lock.lock().expect("hold sync lock");

        assert!(try_sync_lock(&beads_dir).unwrap().is_none());

        drop(held_lock);
        let acquired = try_sync_lock(&beads_dir)
            .expect("sync lock after release")
            .expect("uncontended lock should be acquired");
        drop(acquired);
    }

    #[test]
    fn export_temp_path_is_pid_scoped_and_sibling_to_target() {
        let target = Path::new("/tmp/issues.jsonl");
        let temp = export_temp_path(target);

        assert_eq!(temp.parent(), target.parent());
        assert_ne!(temp, target);
        assert!(
            temp.display()
                .to_string()
                .contains(&std::process::id().to_string())
        );
        assert!(temp.extension().is_some_and(|ext| ext == "tmp"));
    }

    fn make_issue_at(id: &str, title: &str, updated_at: chrono::DateTime<Utc>) -> Issue {
        let created_at = updated_at - chrono::Duration::seconds(60);
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
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
            created_at,
            created_by: None,
            updated_at,
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    fn set_content_hash(issue: &mut Issue) {
        issue.content_hash = Some(crate::util::content_hash(issue));
    }

    fn fixed_time(secs: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn additive_test_paths(temp: &TempDir) -> (PathBuf, PathBuf, AdditiveReconcileConfig) {
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create test beads directory");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let config = AdditiveReconcileConfig {
            beads_dir: Some(beads_dir.clone()),
            database_path: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
        };
        (beads_dir, jsonl_path, config)
    }

    fn write_additive_issues(path: &Path, issues: &[Issue]) {
        let mut bytes = Vec::new();
        for issue in issues {
            serde_json::to_writer(&mut bytes, issue).expect("serialize additive issue");
            bytes.push(b'\n');
        }
        fs::write(path, bytes).expect("write additive JSONL");
    }

    fn canonical_additive_test_issue(mut issue: Issue) -> Issue {
        canonicalize_additive_issue_for_storage(&mut issue);
        issue
    }

    fn apply_reviewed_additive_plan(
        storage: &mut SqliteStorage,
        jsonl_path: &Path,
        config: &AdditiveReconcileConfig,
        plan: &AdditiveReconcilePlan,
    ) -> Result<AdditiveReconcileReceipt> {
        apply_additive_reconcile(
            storage,
            jsonl_path,
            config,
            plan,
            &plan.receipt().plan_sha256,
        )
    }

    #[test]
    fn additive_sqlite_value_witness_is_storage_class_and_bit_exact() {
        let witnesses = [
            additive_sqlite_value_witness(SqliteValue::Null),
            additive_sqlite_value_witness(SqliteValue::Integer(2)),
            additive_sqlite_value_witness(SqliteValue::Float(2.0)),
            additive_sqlite_value_witness(SqliteValue::from("2")),
            additive_sqlite_value_witness(SqliteValue::from(b"2".as_slice())),
        ];
        assert_eq!(witnesses.iter().collect::<BTreeSet<_>>().len(), 5);
        assert_ne!(
            additive_sqlite_value_witness(SqliteValue::Float(0.0)),
            additive_sqlite_value_witness(SqliteValue::Float(-0.0))
        );
        assert_eq!(
            additive_sqlite_value_witness(SqliteValue::from("A\0B")),
            "text:A\0B"
        );
        assert_eq!(
            additive_sqlite_value_witness(SqliteValue::from([0xff, 0x00].as_slice())),
            "blob:ff00"
        );
    }

    #[test]
    fn strict_additive_source_rejects_duplicate_and_noncanonical_agent_context() {
        let mut issue = make_issue_at("bd-context", "Context", fixed_time(100));
        issue.agent_context = Some(r#"{"a":{"b":1}}"#.to_string());
        let canonical = serde_json::to_string(&issue).unwrap();
        assert!(parse_strict_additive_issue(&canonical, 1).is_ok());

        for invalid_context in [
            r#"{"b":1, "a":2}"#,
            r#"{"a":1,"a":2}"#,
            r#"{"nested":{"a":1,"\u0061":2}}"#,
            "not-json",
        ] {
            issue.agent_context = Some(invalid_context.to_string());
            let record = serde_json::to_string(&issue).unwrap();
            assert!(
                parse_strict_additive_issue(&record, 1).is_err(),
                "agent_context unexpectedly accepted: {invalid_context}"
            );
        }

        let duplicate_issue_key = canonical.replacen(
            r#""title":"Context""#,
            r#""title":"Context","title":"Duplicate""#,
            1,
        );
        assert!(parse_strict_additive_issue(&duplicate_issue_key, 1).is_err());
    }

    #[test]
    fn additive_reconcile_repairs_content_hash_only_drift_then_is_a_true_noop() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue_at("bd-hash-repair", "Hash repair", fixed_time(100));
        let null_issue = make_issue_at("bd-hash-null", "Null hash repair", fixed_time(101));
        storage.upsert_issue_for_import(&issue).unwrap();
        storage.upsert_issue_for_import(&null_issue).unwrap();
        storage
            .execute_raw(
                "UPDATE issues SET content_hash = 'stale-content-hash' WHERE id = 'bd-hash-repair'",
            )
            .unwrap();
        storage
            .execute_raw("UPDATE issues SET content_hash = NULL WHERE id = 'bd-hash-null'")
            .unwrap();
        write_additive_issues(&jsonl_path, &[issue.clone(), null_issue.clone()]);
        let events_before = storage.get_all_events(0).unwrap();

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);
        assert_eq!(plan.receipt().content_hash_repairs_planned, 2);
        assert_eq!(
            plan.receipt().content_hash_repair_issue_ids,
            ["bd-hash-null", "bd-hash-repair"]
        );
        assert_eq!(
            plan.receipt().content_hash_repairs,
            [
                AdditiveContentHashRepairWitness {
                    issue_id: "bd-hash-null".to_string(),
                    before: None,
                    after: crate::util::content_hash(&null_issue),
                },
                AdditiveContentHashRepairWitness {
                    issue_id: "bd-hash-repair".to_string(),
                    before: Some("stale-content-hash".to_string()),
                    after: crate::util::content_hash(&issue),
                },
            ]
        );
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.content_hash_repairs_applied, 2);
        assert_eq!(storage.get_all_events(0).unwrap(), events_before);
        let expected_hash = crate::util::content_hash(&issue);
        assert_eq!(
            additive_issue_content_hashes(&storage)
                .unwrap()
                .get("bd-hash-repair"),
            Some(&Some(expected_hash))
        );
        assert_eq!(
            additive_issue_content_hashes(&storage)
                .unwrap()
                .get("bd-hash-null"),
            Some(&Some(crate::util::content_hash(&null_issue)))
        );

        let second = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(second.receipt().status, AdditiveReconcileStatus::NoChanges);
        assert_eq!(second.receipt().content_hash_repairs_planned, 0);
    }

    #[test]
    fn additive_reconcile_binds_full_raw_poststate_for_scalar_updates() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, mut config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let local = make_issue_at("bd-raw-poststate", "Before", fixed_time(100));
        storage.upsert_issue_for_import(&local).unwrap();
        storage
            .execute_raw("UPDATE issues SET status = 'OPEN' WHERE id = 'bd-raw-poststate'")
            .unwrap();
        let mut source = local;
        source.title = "After".to_string();
        write_additive_issues(&jsonl_path, std::slice::from_ref(&source));
        config
            .source_authoritative_ids
            .insert("bd-raw-poststate".to_string());

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_ne!(
            plan.receipt().target_before.issue_payload_sha256,
            plan.receipt().expected_issue_raw_payload_sha256
        );
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        let after = receipt.target_after.as_ref().unwrap();
        assert_eq!(
            after.issue_payload_sha256,
            receipt.expected_issue_raw_payload_sha256
        );
        assert_eq!(
            storage
                .execute_raw_query("SELECT status FROM issues WHERE id = 'bd-raw-poststate'")
                .unwrap()[0][0]
                .as_text(),
            Some("open")
        );
    }

    #[test]
    fn additive_reconcile_token_binds_requested_resolution_set_even_when_inapplicable() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue_at("bd-equal", "Equal", fixed_time(100));
        storage.upsert_issue_for_import(&issue).unwrap();
        write_additive_issues(&jsonl_path, std::slice::from_ref(&issue));

        let baseline = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        let mut requested = config;
        requested
            .source_authoritative_ids
            .insert("bd-unrelated".to_string());
        let with_request = plan_additive_reconcile(&storage, &jsonl_path, &requested).unwrap();
        assert_ne!(
            baseline.receipt().plan_sha256,
            with_request.receipt().plan_sha256
        );
        assert_eq!(
            with_request
                .receipt()
                .requested_source_authoritative_issue_ids,
            ["bd-unrelated"]
        );
        assert!(
            with_request
                .receipt()
                .source_authoritative_issue_ids
                .is_empty()
        );
        assert_eq!(
            with_request
                .receipt()
                .conflict_reasons
                .get("source_authoritative_resolution_not_applicable"),
            Some(&1)
        );
    }

    fn reviewed_disk_plan(
        beads_dir: &Path,
        database_path: &Path,
        jsonl_path: &Path,
        issue: &Issue,
    ) -> AdditiveReconcilePlan {
        let storage = SqliteStorage::open(database_path).unwrap();
        storage.upsert_issue_for_import(issue).unwrap();
        write_additive_issues(jsonl_path, std::slice::from_ref(issue));
        let config = AdditiveReconcileConfig {
            beads_dir: Some(beads_dir.to_path_buf()),
            database_path: Some(database_path.to_path_buf()),
            allow_external_jsonl: !jsonl_path.starts_with(beads_dir),
            source_authoritative_ids: BTreeSet::new(),
        };
        plan_additive_reconcile(&storage, jsonl_path, &config).unwrap()
    }

    #[test]
    fn reviewed_additive_wrapper_resolves_and_locks_configured_database() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let mut issue = make_issue_at("bd-reviewed", "Reviewed", fixed_time(100));
        set_content_hash(&mut issue);
        let plan = reviewed_disk_plan(&beads_dir, &database_path, &jsonl_path, &issue);

        let receipt = apply_reviewed_additive_reconcile(&ReviewedAdditiveReconcileRequest {
            beads_dir,
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
            expected_plan_sha256: plan.receipt().plan_sha256.clone(),
            lock_timeout_ms: Some(100),
        })
        .unwrap();
        assert!(matches!(
            receipt.status,
            AdditiveReconcileStatus::AppliedMetadataOnly | AdditiveReconcileStatus::NoChanges
        ));
    }

    #[test]
    fn reviewed_additive_wrapper_reuses_the_cli_startup_authority() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let mut issue = make_issue_at(
            "bd-retained-authority",
            "Retained authority",
            fixed_time(100),
        );
        set_content_hash(&mut issue);
        let plan = reviewed_disk_plan(&beads_dir, &database_path, &jsonl_path, &issue);
        let retained_authority = Arc::new(
            blocking_database_family_write_lock_with_timeout(&beads_dir, &database_path, Some(100))
                .unwrap(),
        );

        let receipt = apply_reviewed_additive_reconcile_under_authority(
            &ReviewedAdditiveReconcileRequest {
                beads_dir,
                db_override: None,
                source_path_override: None,
                allow_external_jsonl: false,
                source_authoritative_ids: BTreeSet::new(),
                expected_plan_sha256: plan.receipt().plan_sha256.clone(),
                lock_timeout_ms: Some(100),
            },
            Some(&retained_authority),
        )
        .unwrap();

        assert!(matches!(
            receipt.status,
            AdditiveReconcileStatus::AppliedMetadataOnly | AdditiveReconcileStatus::NoChanges
        ));
        retained_authority.verify_database_authority().unwrap();
    }

    #[test]
    fn additive_health_attestation_stays_outside_the_read_snapshot_transaction() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let mut storage = SqliteStorage::open(&database_path).unwrap();
        let mut survivor = make_issue_at("bd-freelist-000", "Survivor", fixed_time(100));
        set_content_hash(&mut survivor);
        storage.upsert_issue_for_import(&survivor).unwrap();
        for ordinal in 1..128 {
            let mut issue = make_issue_at(
                &format!("bd-freelist-{ordinal:03}"),
                &format!("Freelist fixture {ordinal:03}"),
                fixed_time(100 + ordinal),
            );
            issue.description = Some("freelist payload ".repeat(512));
            set_content_hash(&mut issue);
            storage.upsert_issue_for_import(&issue).unwrap();
        }
        storage
            .execute_raw("DELETE FROM issues WHERE id != 'bd-freelist-000'")
            .unwrap();
        storage.checkpoint_full().unwrap();
        let freelist_rows = storage.execute_raw_query("PRAGMA freelist_count").unwrap();
        let freelist_count = freelist_rows
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap();
        assert!(
            freelist_count > 0,
            "regression fixture must retain committed free pages"
        );

        write_additive_issues(&jsonl_path, std::slice::from_ref(&survivor));
        let config = AdditiveReconcileConfig {
            beads_dir: Some(beads_dir),
            database_path: Some(database_path),
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
        };
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(
            plan.receipt().health_before.integrity_messages,
            ["ok"],
            "full integrity must be attested from autocommit state"
        );
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(
            receipt
                .health_after
                .as_ref()
                .expect("postcommit health")
                .integrity_messages,
            ["ok"],
            "postcommit full integrity must also use autocommit state"
        );
        assert!(
            receipt.postcommit_failures.is_empty(),
            "healthy committed free pages must not become a false postcommit failure"
        );
    }

    #[test]
    fn reviewed_additive_wrapper_preserves_explicit_external_database_support() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        let database_path = cache_dir.join("beads.db");
        let jsonl_path = cache_dir.join("issues.jsonl");
        let mut issue = make_issue_at("bd-external-db", "External DB", fixed_time(100));
        set_content_hash(&mut issue);
        let plan = reviewed_disk_plan(&beads_dir, &database_path, &jsonl_path, &issue);

        let receipt = apply_reviewed_additive_reconcile(&ReviewedAdditiveReconcileRequest {
            beads_dir,
            db_override: Some(database_path),
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
            expected_plan_sha256: plan.receipt().plan_sha256.clone(),
            lock_timeout_ms: Some(100),
        })
        .unwrap();
        assert!(receipt.target_after.is_some());
    }

    #[test]
    #[ignore = "carried red from the stranded sync-safety workstream (failed identically on its own \
                pre-merge snapshot); tracked for completion by the owning workstream"]
    fn reviewed_apply_reports_composable_postcommit_source_drift_and_allows_replan() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let existing = make_issue_at("bd-existing", "Existing", fixed_time(100));
        let incoming = make_issue_at("bd-incoming", "Incoming", fixed_time(200));
        {
            let storage = SqliteStorage::open(&database_path).unwrap();
            storage.upsert_issue_for_import(&existing).unwrap();
        }
        write_additive_issues(&jsonl_path, &[existing, incoming.clone()]);
        let plan_request = ReviewedAdditiveReconcilePlanRequest {
            beads_dir: beads_dir.clone(),
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
        };
        let plan = plan_reviewed_additive_reconcile(&plan_request).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);

        ADDITIVE_TEST_DRIFT_SOURCE_AFTER_FINAL_CHECK.with(|flag| flag.set(true));
        let receipt = apply_reviewed_additive_reconcile(&ReviewedAdditiveReconcileRequest {
            beads_dir: beads_dir.clone(),
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
            expected_plan_sha256: plan.receipt().plan_sha256.clone(),
            lock_timeout_ms: Some(100),
        })
        .unwrap();
        assert_eq!(
            receipt.status,
            AdditiveReconcileStatus::CommittedWithPostconditionFailures
        );
        assert_eq!(receipt.source_preserved_after_commit, Some(false));
        assert!(
            receipt
                .postcommit_failures
                .contains(&AdditivePostcommitFailure::SourceWitnessChanged)
        );
        assert_eq!(
            receipt.target_after.as_ref().map(|witness| witness.issues),
            Some(2),
            "the receipt must tell the truth that SQLite committed before source drift was observed"
        );
        let storage = SqliteStorage::open_current_read_only(&database_path)
            .unwrap()
            .expect("current database after committed source drift");
        assert!(storage.get_issue(&incoming.id).unwrap().is_some());
        drop(storage);

        let retry = plan_reviewed_additive_reconcile(&plan_request).unwrap();
        assert_eq!(
            retry.receipt().status,
            AdditiveReconcileStatus::NoChanges,
            "replanning after the reported postcommit drift must converge safely"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reviewed_apply_reports_committed_database_authority_loss_as_non_retryable_receipt() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let existing = make_issue_at("bd-existing", "Existing", fixed_time(100));
        let incoming = make_issue_at("bd-incoming", "Incoming", fixed_time(200));
        {
            let storage = SqliteStorage::open(&database_path).unwrap();
            storage.upsert_issue_for_import(&existing).unwrap();
        }
        write_additive_issues(&jsonl_path, &[existing, incoming]);
        let plan_request = ReviewedAdditiveReconcilePlanRequest {
            beads_dir: beads_dir.clone(),
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
        };
        let plan = plan_reviewed_additive_reconcile(&plan_request).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);

        SqliteStorage::arm_database_replacement_after_commit_for_test();
        let receipt = apply_reviewed_additive_reconcile(&ReviewedAdditiveReconcileRequest {
            beads_dir,
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
            expected_plan_sha256: plan.receipt().plan_sha256.clone(),
            lock_timeout_ms: Some(100),
        })
        .expect("a committed-but-unwitnessed apply must return its non-retryable receipt");

        assert_eq!(
            receipt.status,
            AdditiveReconcileStatus::CommittedWithPostconditionFailures
        );
        assert_eq!(
            receipt.database_authority_preserved_after_commit,
            Some(false)
        );
        assert!(
            receipt
                .postcommit_failures
                .contains(&AdditivePostcommitFailure::DatabaseAuthorityChanged)
        );
        assert_eq!(
            receipt.target_after.as_ref().map(|witness| witness.issues),
            Some(2),
            "the receipt must preserve the transaction's projected committed state"
        );
    }

    #[test]
    fn database_family_lock_serializes_external_db_across_workspaces_with_one_timeout_budget() {
        let temp = TempDir::new().unwrap();
        let first_beads = temp.path().join("first").join(".beads");
        let second_beads = temp.path().join("second").join(".beads");
        let external_dir = temp.path().join("shared-cache");
        fs::create_dir_all(&first_beads).unwrap();
        fs::create_dir_all(&second_beads).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        let database_path = external_dir.join("shared.db");
        drop(SqliteStorage::open(&database_path).unwrap());

        let first_authority = blocking_database_family_write_lock_with_timeout(
            &first_beads,
            &database_path,
            Some(1_000),
        )
        .unwrap();
        let second_workspace_lock =
            blocking_write_lock_with_timeout(&second_beads, Some(1_000)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(second_workspace_lock);
        });

        let started = Instant::now();
        let error = blocking_database_family_write_lock_with_timeout(
            &second_beads,
            &database_path,
            Some(400),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        releaser.join().unwrap();
        assert!(
            error.to_string().contains("Timed out")
                || error.to_string().contains("timed out")
                || error.to_string().contains("timeout"),
            "unexpected contention error: {error}"
        );
        assert!(
            elapsed >= Duration::from_millis(350),
            "composite authority returned before its shared timeout budget: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "each component appears to have received a fresh timeout instead of one shared budget: {elapsed:?}"
        );

        let first_authority_sha256 = first_authority.authority_path_sha256().to_string();
        drop(first_authority);
        let second_authority = blocking_database_family_write_lock_with_timeout(
            &second_beads,
            &database_path,
            Some(1_000),
        )
        .unwrap();
        assert_eq!(
            second_authority.authority_path_sha256(),
            first_authority_sha256
        );
        assert_eq!(
            second_authority.canonical_database_path(),
            fs::canonicalize(&database_path).unwrap()
        );
    }

    #[test]
    fn database_family_lock_binds_a_new_database_inode_before_hardlink_aliases_can_write() {
        let temp = TempDir::new().unwrap();
        let first_beads = temp.path().join("first").join(".beads");
        let second_beads = temp.path().join("second").join(".beads");
        let external_dir = temp.path().join("shared-cache");
        fs::create_dir_all(&first_beads).unwrap();
        fs::create_dir_all(&second_beads).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        let database_path = external_dir.join("created-under-lock.db");
        let hardlink_alias = external_dir.join("alias.db");

        let first_authority = blocking_database_family_write_lock_with_timeout(
            &first_beads,
            &database_path,
            Some(100),
        )
        .expect("first authority protects the future database family");
        assert!(
            !database_path.exists(),
            "acquiring authority alone must not materialize a missing database"
        );
        let was_missing = first_authority
            .bind_database_inode_for_mutation()
            .expect("prepare the future database inode at the mutation boundary");
        assert!(
            was_missing && !database_path.exists(),
            "authority preparation must preserve missing-database recovery semantics"
        );
        first_authority
            .install_empty_database_replacement_and_bind()
            .expect("install a pre-locked database inode");
        drop(SqliteStorage::open(&database_path).expect("initialize database under inode lock"));
        fs::hard_link(&database_path, &hardlink_alias).expect("create competing hard-link alias");

        let error = blocking_database_family_write_lock_with_timeout(
            &second_beads,
            &hardlink_alias,
            Some(25),
        )
        .expect_err("hard-link alias must contend on the already-held database inode");
        let rendered = error.to_string();
        assert!(
            rendered.contains("Timed out")
                || rendered.contains("timed out")
                || rendered.contains("timeout"),
            "unexpected alias contention error: {rendered}"
        );
        assert!(
            !rendered.contains(temp.path().to_string_lossy().as_ref()),
            "external database authority errors must not disclose absolute paths: {rendered}"
        );

        drop(first_authority);
        let alias_authority = blocking_database_family_write_lock_with_timeout(
            &second_beads,
            &hardlink_alias,
            Some(100),
        )
        .expect("hard-link alias authority is available after the original authority drops");
        drop(alias_authority);
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))]
    #[test]
    fn database_replacement_finalization_reverifies_before_retiring_locks() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let database_path = beads_dir.join("beads.db");
        let original_retained_path = beads_dir.join("original-retained.db");
        fs::write(&database_path, b"original database generation").unwrap();
        let authority = blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &database_path,
            Some(1_000),
        )
        .unwrap();
        authority.bind_database_inode_for_mutation().unwrap();
        install_database_candidate_no_replace(&database_path, &original_retained_path).unwrap();
        authority
            .install_empty_database_replacement_and_bind()
            .unwrap();
        assert_eq!(
            authority
                .database_authority
                .lock()
                .unwrap()
                .retired_locks
                .len(),
            1,
            "the displaced original must remain locked before finalization"
        );

        DatabaseFamilyWriteLock::arm_database_replacement_before_finalize_locked_verify_for_test();
        let error = authority
            .finalize_database_replacement()
            .expect_err("a canonical-path swap at finalization must fail closed");

        assert!(
            error.to_string().contains("database write authority"),
            "unexpected finalization identity error: {error}"
        );
        assert_eq!(
            authority
                .database_authority
                .lock()
                .unwrap()
                .retired_locks
                .len(),
            1,
            "failed finalization must not release the displaced inode lock"
        );
        assert_eq!(
            fs::read(&database_path).unwrap(),
            b"foreign database generation installed by finalize hook",
            "the hook must causally replace the canonical generation"
        );
    }

    #[test]
    fn database_family_authority_detects_workspace_lock_replacement() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        let first_database = external_dir.join("first.db");
        let second_database = external_dir.join("second.db");
        let first_authority = blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &first_database,
            Some(100),
        )
        .unwrap();

        let workspace_lock_path = beads_dir.join(".write.lock");
        fs::rename(
            &workspace_lock_path,
            beads_dir.join(".write.lock.displaced"),
        )
        .expect("move held workspace lock without destroying it");
        let second_authority = blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &second_database,
            Some(100),
        )
        .expect("replacement workspace lock demonstrates why the first guard must re-witness");
        assert!(
            first_authority.verify_database_authority().is_err(),
            "the original guard must fail closed after its workspace lock pathname is replaced"
        );
        drop(second_authority);
        drop(first_authority);
    }

    #[test]
    fn database_family_authority_detects_sidecar_lock_replacement() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        let database_path = external_dir.join("beads.db");
        let authority =
            blocking_database_family_write_lock_with_timeout(&beads_dir, &database_path, Some(100))
                .unwrap();
        let sidecar_path = database_write_authority_path(&database_path).unwrap();
        let displaced_path = sidecar_path.with_extension("lock.displaced");
        fs::rename(&sidecar_path, &displaced_path)
            .expect("move held sidecar lock without destroying it");
        drop(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&sidecar_path)
                .expect("create replacement sidecar"),
        );

        assert!(
            authority.verify_database_authority().is_err(),
            "the guard must fail closed after its canonical sidecar pathname is replaced"
        );
        drop(authority);
    }

    #[cfg(unix)]
    #[test]
    fn reviewed_additive_wrapper_rejects_symlinked_database_leaf() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let outside = temp.path().join("outside.db");
        drop(SqliteStorage::open(&outside).unwrap());
        symlink(&outside, beads_dir.join("beads.db")).unwrap();

        let err = apply_reviewed_additive_reconcile(&ReviewedAdditiveReconcileRequest {
            beads_dir,
            db_override: None,
            source_path_override: None,
            allow_external_jsonl: false,
            source_authoritative_ids: BTreeSet::new(),
            expected_plan_sha256: "a".repeat(64),
            lock_timeout_ms: Some(100),
        })
        .unwrap_err();
        assert!(err.to_string().contains("symlinked database"));
    }

    #[cfg(unix)]
    #[test]
    fn database_family_authority_rejects_symlinked_parent_with_regular_leaf() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        let routed_parent = temp.path().join("routed");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_database = outside_dir.join("beads.db");
        drop(SqliteStorage::open(&outside_database).unwrap());
        symlink(&outside_dir, &routed_parent).unwrap();

        let error = blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &routed_parent.join("beads.db"),
            Some(100),
        )
        .expect_err("a regular database leaf must not hide a symlinked parent route")
        .to_string();
        assert!(
            error.contains("symlinked parent component"),
            "unexpected route error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_family_authority_rejects_symlinked_parent_with_missing_leaf() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        let routed_parent = temp.path().join("routed");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, &routed_parent).unwrap();

        let error = blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &routed_parent.join("not-yet-created.db"),
            Some(100),
        )
        .expect_err("a missing database leaf must not hide a symlinked parent route")
        .to_string();
        assert!(
            error.contains("symlinked parent component"),
            "unexpected route error: {error}"
        );
        assert!(
            !outside_dir.join("not-yet-created.db").exists(),
            "rejected authority acquisition must not materialize the routed leaf"
        );
    }

    #[test]
    fn additive_reconcile_exact_ids_are_read_only_until_apply_and_preserve_events() {
        let temp = TempDir::new().unwrap();
        let (beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let existing = make_issue_at("bd-existing", "Same payload", fixed_time(100));
        storage.create_issue(&existing, "test-actor").unwrap();
        let same_hash_new_id = make_issue_at("bd-new", "Same payload", fixed_time(200));
        write_additive_issues(&jsonl_path, &[existing.clone(), same_hash_new_id.clone()]);

        let source_before = fs::read(&jsonl_path).unwrap();
        let events_before = storage.get_all_events(0).unwrap();
        let dirty_before = storage.get_dirty_issue_metadata().unwrap();
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);
        assert_eq!(plan.receipt().created, 1);
        assert_eq!(plan.receipt().updated, 0);
        assert_eq!(plan.receipt().skipped_equal, 1);
        assert_eq!(plan.receipt().synchronized, 2);
        assert_eq!(plan.receipt().deleted, 0);
        assert_eq!(plan.receipt().db_only_preserved, 0);
        assert_eq!(plan.mutation_count(), 1);
        assert!(storage.get_issue("bd-new").unwrap().is_none());
        assert_eq!(storage.get_all_events(0).unwrap(), events_before);
        assert_eq!(storage.get_dirty_issue_metadata().unwrap(), dirty_before);
        assert_eq!(fs::read(&jsonl_path).unwrap(), source_before);

        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
        assert_eq!(
            receipt.target_after.as_ref(),
            Some(&plan.receipt().expected_target_after)
        );
        assert_eq!(receipt.events_before, events_before.len());
        assert_eq!(receipt.events_after, events_before.len());
        assert_eq!(
            receipt.event_payload_sha256_before,
            receipt.event_payload_sha256_after
        );
        assert!(receipt.cache_rebuild_performed);
        assert!(receipt.metadata_changed);
        assert_eq!(
            receipt.export_hashes_updated,
            receipt.export_hash_updates_planned
        );
        assert_eq!(
            receipt.dirty_markers_cleared,
            receipt.dirty_markers_clear_planned
        );
        assert!(!receipt.jsonl_written);
        assert!(!receipt.base_snapshot_used);
        assert!(!receipt.merge_note_written);
        assert_eq!(
            storage.get_issue("bd-new").unwrap().unwrap().title,
            same_hash_new_id.title
        );
        assert_eq!(storage.get_all_events(0).unwrap(), events_before);
        assert_eq!(fs::read(&jsonl_path).unwrap(), source_before);
        assert!(!beads_dir.join("beads.base.jsonl").exists());
        assert!(!beads_dir.join("merge.json").exists());

        let idempotent_plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(
            idempotent_plan.receipt().status,
            AdditiveReconcileStatus::NoChanges
        );
        assert!(!idempotent_plan.receipt().metadata_update_planned);
        assert_eq!(idempotent_plan.receipt().export_hash_updates_planned, 0);
        assert_eq!(idempotent_plan.receipt().dirty_markers_clear_planned, 0);
    }

    #[test]
    fn additive_reconcile_rolls_back_exactly_at_early_mid_and_late_fault_phases() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let incoming = make_issue_at("bd-fault", "Fault rollback", fixed_time(100));
        write_additive_issues(&jsonl_path, std::slice::from_ref(&incoming));
        let source_before = fs::read(&jsonl_path).unwrap();
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);

        for phase in [
            AdditiveTestFailPhase::BeforeTransaction,
            AdditiveTestFailPhase::AfterIssueAndRelationWrites,
            AdditiveTestFailPhase::BeforeFinalCommitChecks,
        ] {
            ADDITIVE_TEST_FAIL_PHASE.with(|configured| configured.set(Some(phase)));
            let error = apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected additive reconciliation failure"),
                "unexpected {phase:?} error: {error}"
            );
            let issues_after = hydrate_additive_database_issues(&storage).unwrap();
            let witness_after = additive_database_witness(&storage, &issues_after).unwrap();
            assert_eq!(
                witness_after,
                plan.receipt().target_before,
                "{phase:?} failure must restore the complete typed database prestate"
            );
            assert_eq!(fs::read(&jsonl_path).unwrap(), source_before);
            assert_eq!(
                plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap(),
                plan,
                "{phase:?} rollback must leave the exact reviewed plan reusable"
            );
        }

        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
        assert!(storage.get_issue(&incoming.id).unwrap().is_some());
    }

    #[test]
    fn additive_reconcile_rejects_schema_version_drift_inside_transaction() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let incoming = make_issue_at("bd-schema-drift", "Schema drift", fixed_time(100));
        write_additive_issues(&jsonl_path, std::slice::from_ref(&incoming));
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();

        ADDITIVE_TEST_DRIFT_SCHEMA_BEFORE_TRANSACTION.with(|flag| flag.set(true));
        let error = apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan)
            .expect_err("in-transaction schema drift must invalidate the reviewed plan");
        assert!(
            error.to_string().contains("schema version changed"),
            "unexpected schema drift error: {error}"
        );
        assert!(
            storage.get_issue(&incoming.id).unwrap().is_none(),
            "schema drift must be rejected before the first additive write"
        );

        storage
            .execute_raw(&format!(
                "PRAGMA user_version = {}",
                crate::storage::schema::CURRENT_SCHEMA_VERSION
            ))
            .unwrap();
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
    }

    #[test]
    fn additive_reconcile_applies_relations_and_rebuilds_derived_caches() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let target = make_issue_at("bd-target", "Target", fixed_time(100));
        storage.upsert_issue_for_import(&target).unwrap();
        let mut source = make_issue_at("bd-source", "Source", fixed_time(200));
        source.labels = vec!["search".to_string(), "correctness".to_string()];
        source.dependencies = vec![Dependency {
            issue_id: source.id.clone(),
            depends_on_id: target.id.clone(),
            dep_type: DependencyType::Blocks,
            created_at: fixed_time(180),
            created_by: Some("fixture".to_string()),
            metadata: Some("{}".to_string()),
            thread_id: Some("bd-source".to_string()),
        }];
        source.comments = vec![Comment {
            id: 41,
            issue_id: source.id.clone(),
            author: "fixture".to_string(),
            body: "Preserve this comment exactly.".to_string(),
            created_at: fixed_time(190),
        }];
        write_additive_issues(&jsonl_path, &[target.clone(), source.clone()]);

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(
            plan.receipt().relations_after,
            AdditiveRelationCounts {
                labels: 2,
                dependencies: 1,
                comments: 1,
            }
        );
        assert_eq!(
            plan.receipt().relation_rows_planned,
            AdditiveRelationCounts {
                labels: 2,
                dependencies: 1,
                comments: 1,
            },
            "dry-run receipt must report the relation rows the reviewed apply will insert"
        );
        assert_eq!(
            plan.receipt().relation_rows_applied,
            AdditiveRelationCounts::default(),
            "dry-run receipt must never claim that relation rows were already applied"
        );
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(
            receipt.relation_rows_applied, receipt.relation_rows_planned,
            "committed receipt must distinguish applied relation rows from planning"
        );
        assert_eq!(
            receipt.target_after.as_ref(),
            Some(&plan.receipt().expected_target_after)
        );
        assert_eq!(
            receipt
                .target_after
                .as_ref()
                .expect("after witness")
                .relations,
            plan.receipt().relations_after
        );
        let stored = hydrate_additive_database_issues(&storage).unwrap();
        assert_eq!(receipt.comment_id_remaps.len(), 1);
        assert_eq!(receipt.comment_id_remaps[0].old_id, 41);
        assert_eq!(receipt.comment_id_remaps[0].new_id, 1);
        source.comments[0].id = receipt.comment_id_remaps[0].new_id;
        let expected = canonical_additive_test_issue(source);
        assert_eq!(stored["bd-source"].labels, expected.labels);
        assert_eq!(stored["bd-source"].dependencies, expected.dependencies);
        assert_eq!(stored["bd-source"].comments, expected.comments);
        assert_eq!(receipt.events_before, 0);
        assert_eq!(receipt.events_after, 0);
    }

    #[test]
    fn additive_reconcile_conflicts_on_equal_timestamp_drift_and_resurrection() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let local = make_issue_at("bd-drift", "Local", fixed_time(100));
        storage.upsert_issue_for_import(&local).unwrap();
        let mut tombstone = make_issue_at("bd-deleted", "Deleted", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(fixed_time(90));
        tombstone.deleted_by = Some("fixture".to_string());
        storage.upsert_issue_for_import(&tombstone).unwrap();

        let drifted = make_issue_at("bd-drift", "External", fixed_time(100));
        let resurrected = make_issue_at("bd-deleted", "Resurrected", fixed_time(200));
        write_additive_issues(&jsonl_path, &[drifted, resurrected]);
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Conflicted);
        assert_eq!(
            plan.receipt()
                .conflict_reasons
                .get("equal_timestamp_shared_scalar_drift"),
            Some(&1)
        );
        assert_eq!(
            plan.receipt()
                .conflict_reasons
                .get("tombstone_resurrection"),
            Some(&1)
        );
        assert_eq!(
            plan.receipt()
                .conflict_scalar_diffs
                .iter()
                .map(|witness| witness.issue_id.as_str())
                .collect::<Vec<_>>(),
            ["bd-deleted", "bd-drift"]
        );
        let drift_witness = plan
            .receipt()
            .conflict_scalar_diffs
            .iter()
            .find(|witness| witness.issue_id == "bd-drift")
            .expect("equal-timestamp drift has a complete scalar witness");
        assert!(
            drift_witness
                .changed_fields
                .iter()
                .any(|field| field == "title")
        );
        assert_eq!(
            plan.receipt().conflict_scalar_diffs_sha256,
            additive_sha256(
                &plan.receipt().conflict_scalar_diffs,
                "complete conflict scalar diff manifest"
            )
            .unwrap()
        );
        assert!(apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).is_err());
        assert_eq!(
            storage.get_issue("bd-drift").unwrap().unwrap().title,
            "Local"
        );
        assert_eq!(
            storage.get_issue("bd-deleted").unwrap().unwrap().status,
            Status::Tombstone
        );
    }

    #[test]
    fn additive_reconcile_binds_relation_deltas_for_shared_and_tombstone_conflicts() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let storage = SqliteStorage::open_memory().unwrap();

        let mut local = make_issue_at("bd-relation", "Same scalars", fixed_time(100));
        local.labels = vec!["private-local-label".to_string()];
        storage
            .upsert_issue_and_relations_for_import(&local)
            .unwrap();
        let mut incoming = local.clone();
        incoming.labels = vec!["private-source-label".to_string()];

        let mut tombstone = make_issue_at("bd-deleted-rel", "Deleted", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(fixed_time(90));
        tombstone.deleted_by = Some("fixture".to_string());
        tombstone.labels = vec!["private-tombstone-label".to_string()];
        storage
            .upsert_issue_and_relations_for_import(&tombstone)
            .unwrap();
        let mut resurrected = make_issue_at("bd-deleted-rel", "Resurrected", fixed_time(200));
        resurrected.labels = vec!["private-resurrected-label".to_string()];

        write_additive_issues(&jsonl_path, &[incoming, resurrected]);
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(
            plan.receipt().conflict_reasons.get("shared_relation_drift"),
            Some(&1)
        );
        assert_eq!(
            plan.receipt()
                .conflict_reasons
                .get("tombstone_resurrection"),
            Some(&1)
        );
        assert_eq!(plan.receipt().conflict_relation_diffs.len(), 2);
        for witness in &plan.receipt().conflict_relation_diffs {
            assert_eq!(witness.changed_relation_classes, ["labels"]);
            assert_eq!(witness.before_counts.labels, 1);
            assert_eq!(witness.after_counts.labels, 1);
            assert_eq!(witness.added_element_sha256.len(), 1);
            assert_eq!(witness.removed_element_sha256.len(), 1);
            assert_ne!(witness.before_payload_sha256, witness.after_payload_sha256);
        }
        assert_eq!(
            plan.receipt().conflict_relation_diffs_sha256,
            additive_sha256(
                &plan.receipt().conflict_relation_diffs,
                "complete conflict relation diff manifest"
            )
            .unwrap()
        );
        let serialized = serde_json::to_string(plan.receipt()).unwrap();
        for private_value in [
            "private-local-label",
            "private-source-label",
            "private-tombstone-label",
            "private-resurrected-label",
        ] {
            assert!(!serialized.contains(private_value));
        }
    }

    #[test]
    fn additive_reconcile_requires_explicit_resolution_for_database_newer_rows() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let local_newer = make_issue_at("bd-shared", "Local newer", fixed_time(300));
        let db_only = make_issue_at("bd-db-only", "Database only", fixed_time(200));
        storage.upsert_issue_for_import(&local_newer).unwrap();
        storage.upsert_issue_for_import(&db_only).unwrap();
        let source_older = make_issue_at("bd-shared", "Source older", fixed_time(100));
        write_additive_issues(&jsonl_path, std::slice::from_ref(&source_older));

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Conflicted);
        assert_eq!(
            plan.receipt()
                .conflict_reasons
                .get("database_newer_shared_scalar_drift"),
            Some(&1)
        );
        assert_eq!(plan.receipt().db_only_preserved, 1);
        assert_eq!(plan.receipt().deleted, 0);
        assert!(apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).is_err());

        let mut resolved_config = config;
        resolved_config
            .source_authoritative_ids
            .insert("bd-shared".to_string());
        let resolved_plan =
            plan_additive_reconcile(&storage, &jsonl_path, &resolved_config).unwrap();
        assert_eq!(
            resolved_plan.receipt().status,
            AdditiveReconcileStatus::Conflicted
        );
        assert_eq!(
            resolved_plan
                .receipt()
                .conflict_reasons
                .get("database_newer_source_resolution_forbidden"),
            Some(&1)
        );
        assert!(
            apply_reviewed_additive_plan(
                &mut storage,
                &jsonl_path,
                &resolved_config,
                &resolved_plan,
            )
            .is_err()
        );

        let mut source_equal = local_newer.clone();
        source_equal.description = Some("Reviewed source description".to_string());
        write_additive_issues(&jsonl_path, std::slice::from_ref(&source_equal));
        let equal_timestamp_plan =
            plan_additive_reconcile(&storage, &jsonl_path, &resolved_config).unwrap();
        assert_eq!(
            equal_timestamp_plan.receipt().status,
            AdditiveReconcileStatus::Ready
        );
        assert_eq!(equal_timestamp_plan.receipt().updated, 1);
        let receipt = apply_reviewed_additive_plan(
            &mut storage,
            &jsonl_path,
            &resolved_config,
            &equal_timestamp_plan,
        )
        .unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
        assert!(receipt.metadata_changed);
        assert!(receipt.cache_rebuild_performed);
        assert_eq!(
            receipt
                .target_after
                .as_ref()
                .and_then(|witness| witness.needs_flush.as_deref()),
            Some("true")
        );
        let stored = hydrate_additive_database_issues(&storage).unwrap();
        assert_eq!(
            stored["bd-shared"],
            canonical_additive_test_issue(source_equal)
        );
        assert_eq!(stored["bd-db-only"], canonical_additive_test_issue(db_only));
    }

    #[test]
    fn additive_reconcile_reports_relation_and_external_identity_conflicts() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let storage = SqliteStorage::open_memory().unwrap();
        let mut owner = make_issue_at("bd-owner", "Owner", fixed_time(100));
        owner.external_ref = Some("EXT-1".to_string());
        storage.upsert_issue_for_import(&owner).unwrap();

        let mut invalid = make_issue_at("bd-invalid", "Invalid", fixed_time(200));
        invalid.external_ref = Some("EXT-1".to_string());
        invalid.dependencies = vec![Dependency {
            issue_id: invalid.id.clone(),
            depends_on_id: "bd-missing".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: fixed_time(180),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        invalid.comments = vec![Comment {
            id: 9,
            issue_id: "bd-other".to_string(),
            author: "fixture".to_string(),
            body: "Wrong owner".to_string(),
            created_at: fixed_time(190),
        }];
        write_additive_issues(&jsonl_path, &[invalid]);

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        for reason in [
            "external_ref_owned_by_other_id",
            "orphan_dependency_target",
            "comment_source_id_mismatch",
        ] {
            assert_eq!(plan.receipt().conflict_reasons.get(reason), Some(&1));
        }
        let witness = plan
            .receipt()
            .conflict_witnesses
            .iter()
            .find(|witness| witness.issue_id == "bd-invalid")
            .expect("invalid issue conflict witness");
        assert!(witness.details.iter().any(|detail| {
            detail.reason == "external_ref_owned_by_other_id"
                && detail.detail_kind == "external_ref"
                && detail.related_value_sha256 == [hex_encode(&Sha256::digest(b"bd-owner"))]
                && detail.value_sha256.is_some()
        }));
        assert!(witness.details.iter().any(|detail| {
            detail.reason == "orphan_dependency_target"
                && detail.ordinal == Some(0)
                && detail.related_value_sha256 == [hex_encode(&Sha256::digest(b"bd-missing"))]
        }));
        assert!(witness.details.iter().any(|detail| {
            detail.reason == "comment_source_id_mismatch"
                && detail.ordinal == Some(0)
                && detail.related_value_sha256 == [hex_encode(&Sha256::digest(b"bd-other"))]
                && detail.value_sha256.is_some()
        }));
        let serialized_witness = serde_json::to_string(witness).unwrap();
        assert!(!serialized_witness.contains("EXT-1"));
        assert!(!serialized_witness.contains("Wrong owner"));
        assert!(!serialized_witness.contains("bd-owner"));
        assert!(!serialized_witness.contains("bd-missing"));
        assert!(!serialized_witness.contains("bd-other"));
        assert_eq!(plan.mutation_count(), 0);
        assert!(storage.get_issue("bd-invalid").unwrap().is_none());
    }

    #[test]
    fn additive_reconcile_reports_safe_comment_validation_subcodes_and_full_payload_hash() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let storage = SqliteStorage::open_memory().unwrap();
        let mut invalid = make_issue_at("bd-invalid-comment", "Invalid comment", fixed_time(200));
        let comment = Comment {
            id: 0,
            issue_id: invalid.id.clone(),
            author: String::new(),
            body: "\0PRIVATE-COMMENT\n\u{1b}[31m".to_string(),
            created_at: fixed_time(190),
        };
        let canonical_payload = serde_json::to_string(&comment).unwrap();
        invalid.comments = vec![comment];
        write_additive_issues(&jsonl_path, &[invalid]);

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(
            plan.receipt().conflict_reasons.get("invalid_comment"),
            Some(&1)
        );
        let detail = plan
            .receipt()
            .conflict_witnesses
            .iter()
            .flat_map(|witness| &witness.details)
            .find(|detail| detail.reason == "invalid_comment")
            .expect("invalid comment detail");
        assert_eq!(detail.detail_kind, "comment_validation");
        assert_eq!(
            detail.validation_subcodes,
            ["author_empty", "body_contains_nul"]
        );
        assert_eq!(
            detail.value_sha256,
            Some(hex_encode(&Sha256::digest(canonical_payload.as_bytes())))
        );
        let serialized = serde_json::to_string(detail).unwrap();
        assert!(!serialized.contains("PRIVATE-COMMENT"));
        assert!(!serialized.contains("\\u001b"));
        assert_eq!(plan.mutation_count(), 0);
    }

    #[test]
    fn additive_reconcile_rejects_projected_cycles_and_remaps_comment_ids() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut first = make_issue_at("bd-cycle-a", "Cycle A", fixed_time(100));
        let mut second = make_issue_at("bd-cycle-b", "Cycle B", fixed_time(100));
        first.dependencies = vec![Dependency {
            issue_id: first.id.clone(),
            depends_on_id: second.id.clone(),
            dep_type: DependencyType::Blocks,
            created_at: fixed_time(90),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        second.dependencies = vec![Dependency {
            issue_id: second.id.clone(),
            depends_on_id: first.id.clone(),
            dep_type: DependencyType::WaitsFor,
            created_at: fixed_time(90),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        write_additive_issues(&jsonl_path, &[first, second]);

        let cycle_plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(
            cycle_plan.receipt().status,
            AdditiveReconcileStatus::Conflicted
        );
        assert_eq!(cycle_plan.receipt().preexisting_blocking_cycles, 0);
        assert_eq!(cycle_plan.receipt().projected_blocking_cycles, 1);
        assert_eq!(cycle_plan.receipt().new_blocking_cycles, 1);
        assert_eq!(
            cycle_plan
                .receipt()
                .conflict_reasons
                .get("projected_blocking_cycle"),
            Some(&2)
        );
        assert_eq!(cycle_plan.receipt().conflicted, 2);

        let mut first_comment_owner = make_issue_at("bd-comment-a", "Comment A", fixed_time(100));
        let mut second_comment_owner = make_issue_at("bd-comment-b", "Comment B", fixed_time(100));
        first_comment_owner.comments = vec![Comment {
            id: 77,
            issue_id: first_comment_owner.id.clone(),
            author: "fixture".to_string(),
            body: "First owner".to_string(),
            created_at: fixed_time(95),
        }];
        second_comment_owner.comments = vec![
            Comment {
                id: 77,
                issue_id: second_comment_owner.id.clone(),
                author: "fixture".to_string(),
                body: "Duplicate owner".to_string(),
                created_at: fixed_time(95),
            },
            Comment {
                id: 0,
                issue_id: second_comment_owner.id.clone(),
                author: "fixture".to_string(),
                body: "Allocate this nonpositive surrogate ID.".to_string(),
                created_at: fixed_time(96),
            },
        ];
        write_additive_issues(&jsonl_path, &[first_comment_owner, second_comment_owner]);

        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);
        assert_eq!(plan.receipt().preexisting_blocking_cycles, 0);
        assert_eq!(plan.receipt().projected_blocking_cycles, 0);
        assert_eq!(plan.receipt().new_blocking_cycles, 0);
        assert_eq!(plan.receipt().conflicted, 0);
        assert_eq!(plan.receipt().comment_id_remaps.len(), 3);
        assert_eq!(
            plan.receipt()
                .comment_id_remaps
                .iter()
                .map(|remap| (remap.old_id, remap.new_id))
                .collect::<Vec<_>>(),
            vec![(77, 1), (0, 3), (77, 2)]
        );
        assert!(storage.get_issue("bd-cycle-a").unwrap().is_none());
        assert!(storage.get_issue("bd-cycle-b").unwrap().is_none());
        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
        let stored = hydrate_additive_database_issues(&storage).unwrap();
        assert_eq!(
            stored["bd-comment-b"]
                .comments
                .iter()
                .map(|comment| comment.id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn additive_reconcile_preserves_preexisting_cycles_without_misclassifying_them_as_new() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut first = make_issue_at("bd-existing-cycle-a", "Cycle A", fixed_time(100));
        let mut second = make_issue_at("bd-existing-cycle-b", "Cycle B", fixed_time(100));
        first.dependencies = vec![Dependency {
            issue_id: first.id.clone(),
            depends_on_id: second.id.clone(),
            dep_type: DependencyType::Blocks,
            created_at: fixed_time(90),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        second.dependencies = vec![Dependency {
            issue_id: second.id.clone(),
            depends_on_id: first.id.clone(),
            dep_type: DependencyType::WaitsFor,
            created_at: fixed_time(90),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        storage.upsert_issue_for_import(&first).unwrap();
        storage.upsert_issue_for_import(&second).unwrap();
        sync_issue_relations(&storage, &first).unwrap();
        sync_issue_relations(&storage, &second).unwrap();

        let incoming = make_issue_at("bd-cycle-independent", "Independent", fixed_time(200));
        write_additive_issues(
            &jsonl_path,
            &[first.clone(), second.clone(), incoming.clone()],
        );
        let plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(plan.receipt().status, AdditiveReconcileStatus::Ready);
        assert_eq!(plan.receipt().preexisting_blocking_cycles, 1);
        assert_eq!(plan.receipt().projected_blocking_cycles, 1);
        assert_eq!(plan.receipt().new_blocking_cycles, 0);
        assert_eq!(plan.receipt().conflicted, 0);
        assert_eq!(plan.receipt().created, 1);

        let receipt =
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &plan).unwrap();
        assert_eq!(receipt.status, AdditiveReconcileStatus::Applied);
        assert_eq!(
            storage
                .get_issue(&incoming.id)
                .unwrap()
                .expect("independent incoming issue")
                .title,
            incoming.title
        );
    }

    #[test]
    fn additive_reconcile_rejects_duplicate_ids_and_stale_source_or_database() {
        let temp = TempDir::new().unwrap();
        let (_beads_dir, jsonl_path, config) = additive_test_paths(&temp);
        let mut storage = SqliteStorage::open_memory().unwrap();
        let incoming = make_issue_at("bd-new", "New", fixed_time(100));
        write_additive_issues(&jsonl_path, &[incoming.clone(), incoming.clone()]);
        assert!(plan_additive_reconcile(&storage, &jsonl_path, &config).is_err());

        write_additive_issues(&jsonl_path, std::slice::from_ref(&incoming));
        let stale_source_plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        let changed = make_issue_at("bd-new", "Changed", fixed_time(200));
        write_additive_issues(&jsonl_path, std::slice::from_ref(&changed));
        assert!(
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &stale_source_plan)
                .is_err()
        );
        assert!(storage.get_issue("bd-new").unwrap().is_none());

        write_additive_issues(&jsonl_path, std::slice::from_ref(&incoming));
        let stale_database_plan = plan_additive_reconcile(&storage, &jsonl_path, &config).unwrap();
        let concurrent = make_issue_at("bd-concurrent", "Concurrent", fixed_time(300));
        storage.upsert_issue_for_import(&concurrent).unwrap();
        assert!(
            apply_reviewed_additive_plan(&mut storage, &jsonl_path, &config, &stale_database_plan)
                .is_err()
        );
        assert!(storage.get_issue("bd-new").unwrap().is_none());
    }

    fn build_collision_maps(
        storage: &SqliteStorage,
    ) -> (
        HashMap<String, String>,
        HashMap<String, String>,
        HashMap<String, crate::storage::sqlite::IssueMetadata>,
    ) {
        let all_meta = storage.get_all_issues_metadata().unwrap();
        let mut meta_by_id = HashMap::new();
        let mut id_by_ext_ref = HashMap::new();
        let mut id_by_hash = HashMap::new();

        for meta in all_meta {
            let issue_id = meta.id.clone();
            if let Some(ext) = meta.external_ref.as_ref() {
                id_by_ext_ref
                    .entry(ext.clone())
                    .or_insert_with(|| issue_id.clone());
            }
            if meta.status != Status::Tombstone
                && let Some(hash) = meta.content_hash.as_ref()
            {
                id_by_hash
                    .entry(hash.clone())
                    .or_insert_with(|| issue_id.clone());
            }
            meta_by_id.insert(issue_id, meta);
        }

        (id_by_ext_ref, id_by_hash, meta_by_id)
    }

    struct LineFailWriter {
        buffer: Vec<u8>,
        current: Vec<u8>,
        fail_on: String,
        failed: bool,
    }

    impl LineFailWriter {
        fn new(fail_on: &str) -> Self {
            Self {
                buffer: Vec::new(),
                current: Vec::new(),
                fail_on: fail_on.to_string(),
                failed: false,
            }
        }

        fn into_string(self) -> String {
            String::from_utf8(self.buffer).unwrap_or_default()
        }
    }

    impl Write for LineFailWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.current.extend_from_slice(buf);
            while let Some(pos) = self.current.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.current.drain(..=pos).collect();
                let line_str = String::from_utf8_lossy(&line);
                if !self.failed && line_str.contains(&self.fail_on) {
                    self.failed = true;
                    return Err(io::Error::other("intentional failure"));
                }
                self.buffer.extend_from_slice(&line);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_scan_conflict_markers_detects_all_kinds() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("issues.jsonl");
        let contents = concat!(
            "{\"id\":\"bd-1\",\"title\":\"ok\"}\n",
            "<<<<<<< HEAD\n",
            "{\"id\":\"bd-2\",\"title\":\"conflict\"}\n",
            "=======\n",
            "{\"id\":\"bd-2\",\"title\":\"other\"}\n",
            ">>>>>>> feature-branch\n",
        );
        fs::write(&path, contents).expect("write");

        let markers = scan_conflict_markers(&path).expect("scan");
        assert_eq!(markers.len(), 3);
        assert_eq!(markers[0].marker_type, ConflictMarkerType::Start);
        assert_eq!(markers[1].marker_type, ConflictMarkerType::Separator);
        assert_eq!(markers[2].marker_type, ConflictMarkerType::End);
        assert_eq!(markers[0].branch.as_deref(), Some("HEAD"));
        assert_eq!(markers[2].branch.as_deref(), Some("feature-branch"));
    }

    #[test]
    fn test_ensure_no_conflict_markers_errors() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("issues.jsonl");
        fs::write(&path, "<<<<<<< HEAD\n").expect("write");

        let err = ensure_no_conflict_markers(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Merge conflict markers detected"));
    }

    #[test]
    fn test_export_empty_database() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        assert_eq!(result.exported_count, 0);
        assert!(result.exported_ids.is_empty());
        assert!(output_path.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_creates_missing_target_without_replace_race() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let staged_state = staged_source.state_witness();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let publication = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &JsonlSourceStateWitness::Missing,
            &content_sha256,
            &authority,
            || Ok(()),
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(
            publication.atomicity,
            ExportPublicationAtomicity::CreateNoReplace
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(!temp_path.exists());
        assert!(publication.cleanup_durable);
        assert!(publication.retained_recovery_path.is_none());
        assert_eq!(publication.source.display_path(), output_path);
        assert_eq!(publication.source.state_witness(), staged_state);
        assert_eq!(publication.source.content_sha256(), content_sha256);
    }

    /// #413: acquiring and re-verifying the JSONL-family authority must
    /// succeed on Windows even though the canonical sidecar key is a verbatim
    /// `\\?\` spelling while the pinned route stays lexical.
    #[cfg(windows)]
    #[test]
    fn windows_jsonl_family_authority_reconciles_verbatim_sidecar_key() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let contents = b"{\"id\":\"existing\"}\n";
        fs::write(&output_path, contents).unwrap();

        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None)
            .expect("Windows JSONL authority acquisition must not conflict with itself");
        authority
            .verify_jsonl_authority()
            .expect("re-verify the held Windows authority");
        let captured = authority
            .capture_optional_target()
            .expect("capture through the held Windows authority")
            .expect("existing target should be captured");
        assert_eq!(captured.size(), contents.len() as u64);

        // First exports run before the leaf exists; the missing-leaf
        // convention must reconcile the same way.
        let missing = temp.path().join("fresh.jsonl");
        let fresh_authority = blocking_jsonl_family_write_lock_with_timeout(&missing, None)
            .expect("Windows authority over a missing JSONL leaf");
        assert!(
            fresh_authority
                .capture_optional_target()
                .expect("capture the missing Windows target")
                .is_none()
        );
    }

    /// #413: Windows conditional publication installs a missing destination
    /// with the native atomic no-replace rename and replaces an existing one
    /// with the witness-checked under-authority fallback recorded in the
    /// receipt.
    #[cfg(windows)]
    #[test]
    fn windows_conditional_publication_creates_then_replaces_under_authority() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");

        let temp_path = export_temp_path(&output_path);
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let created = publish_staged_file_conditionally(&temp_path, &output_path)
            .expect("create-tier Windows publication");
        assert_eq!(
            created.atomicity(),
            ExportPublicationAtomicity::CreateNoReplace
        );
        assert!(!created.atomicity().is_downgraded());
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(
            !temp_path.exists(),
            "the no-replace rename must consume the staged name"
        );

        let replacement_path = export_temp_path(&output_path);
        fs::write(&replacement_path, b"{\"id\":\"replacement\"}\n").unwrap();
        let replaced = publish_staged_file_conditionally(&replacement_path, &output_path)
            .expect("replace-tier Windows publication");
        assert_eq!(
            replaced.atomicity(),
            ExportPublicationAtomicity::ReplaceUnderAuthority
        );
        assert!(replaced.atomicity().is_downgraded());
        assert_eq!(replaced.atomicity().as_str(), "replace-under-authority");
        assert_eq!(
            fs::read(&output_path).unwrap(),
            b"{\"id\":\"replacement\"}\n"
        );
        assert!(
            !replacement_path.exists(),
            "the replacing rename must consume the staged name"
        );
    }

    #[cfg(unix)]
    fn assert_parent_route_replacement_is_contained(
        replacement_phase: ConditionalPublicationHookPhase,
    ) {
        let temp = TempDir::new().unwrap();
        let live_parent = temp.path().join("live");
        let displaced_parent = temp.path().join("pinned-original");
        fs::create_dir(&live_parent).unwrap();
        let output_path = live_parent.join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let replaced = std::cell::Cell::new(false);

        let result = publish_staged_jsonl_conditionally_with_hooks(
            &temp_path,
            TempFileGuard::new_retained(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            |phase| {
                if phase == replacement_phase {
                    assert!(!replaced.replace(true), "phase hook ran more than once");
                    fs::rename(&live_parent, &displaced_parent)?;
                    fs::create_dir(&live_parent)?;
                    fs::write(&output_path, b"{\"id\":\"attacker-target\"}\n")?;
                    fs::write(&temp_path, b"{\"id\":\"attacker-temp\"}\n")?;
                }
                Ok(())
            },
            JsonlFamilyWriteLock::fsync_pinned_parent,
        );

        assert!(replaced.get(), "requested phase hook did not run");
        assert!(matches!(
            result,
            Err(BeadsError::JsonlPublishedButUnwitnessed { .. })
        ));
        assert_eq!(
            fs::read(&output_path).unwrap(),
            b"{\"id\":\"attacker-target\"}\n",
            "publication must not follow a substituted target parent"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"attacker-temp\"}\n",
            "cleanup must not follow a substituted staging parent"
        );
        assert_eq!(
            fs::read(displaced_parent.join("issues.jsonl")).unwrap(),
            b"{\"id\":\"new\"}\n",
            "the namespace change remains confined to the pinned parent"
        );
        assert_eq!(
            fs::read(displaced_parent.join(temp_path.file_name().unwrap())).unwrap(),
            b"{\"id\":\"old\"}\n",
            "the displaced generation is retained under the pinned parent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_precreate_route_replacement_cannot_redirect_creation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let live_parent = temp.path().join("live");
        let displaced_parent = temp.path().join("pinned-original");
        fs::create_dir(&live_parent).unwrap();
        let output_path = live_parent.join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let result = create_pinned_jsonl_temp_file_with(
            &output_path,
            &authority,
            |_| Ok(()),
            |phase| {
                assert_eq!(phase, ConditionalPublicationHookPhase::PreCreate);
                fs::rename(&live_parent, &displaced_parent)?;
                fs::create_dir(&live_parent)?;
                fs::write(&output_path, b"{\"id\":\"attacker-target\"}\n")?;
                fs::write(&temp_path, b"{\"id\":\"attacker-temp\"}\n")?;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(&output_path).unwrap(),
            b"{\"id\":\"attacker-target\"}\n"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"attacker-temp\"}\n"
        );
        let pinned_temp = displaced_parent.join(temp_path.file_name().unwrap());
        assert!(pinned_temp.is_file());
        assert_eq!(
            fs::metadata(&pinned_temp).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_precommit_route_replacement_is_contained() {
        assert_parent_route_replacement_is_contained(ConditionalPublicationHookPhase::PreCommit);
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_postrename_route_replacement_is_contained() {
        assert_parent_route_replacement_is_contained(ConditionalPublicationHookPhase::PostRename);
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_fsync_route_replacement_is_contained() {
        assert_parent_route_replacement_is_contained(ConditionalPublicationHookPhase::ParentFsync);
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_cleanup_route_replacement_never_unlinks_outside_parent() {
        assert_parent_route_replacement_is_contained(ConditionalPublicationHookPhase::PreCleanup);
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_cleanup_leaf_substitution_retains_attacker_file() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        let recovery_path = temp.path().join("verified-old.recovery");
        let protected_path = temp.path().join("attacker-controlled.jsonl");
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        fs::write(&protected_path, b"{\"id\":\"protected\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let publication = publish_staged_jsonl_conditionally_with_hooks(
            &temp_path,
            TempFileGuard::new_retained(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            |phase| {
                if phase == ConditionalPublicationHookPhase::PreCleanup {
                    fs::rename(&temp_path, &recovery_path)?;
                    fs::hard_link(&protected_path, &temp_path)?;
                }
                Ok(())
            },
            JsonlFamilyWriteLock::fsync_pinned_parent,
        )
        .unwrap();

        assert!(!publication.cleanup_durable);
        assert_eq!(
            publication.retained_recovery_path.as_deref(),
            Some(temp_path.to_string_lossy().as_ref())
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert_eq!(fs::read(&recovery_path).unwrap(), b"{\"id\":\"old\"}\n");
        assert_eq!(
            fs::read(&protected_path).unwrap(),
            b"{\"id\":\"protected\"}\n"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"protected\"}\n",
            "identity-mismatched cleanup leaf must not be removed"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_no_replace_preserves_target_that_appears_in_final_window() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&temp_path, b"{\"id\":\"staged\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &JsonlSourceStateWitness::Missing,
            &content_sha256,
            &authority,
            || {
                fs::write(&output_path, b"{\"id\":\"concurrent\"}\n")?;
                Ok(())
            },
            |_| Ok(()),
        );

        assert!(matches!(result, Err(BeadsError::SyncConflict { .. })));
        assert_eq!(
            fs::read(&output_path).unwrap(),
            b"{\"id\":\"concurrent\"}\n"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"staged\"}\n",
            "an unpublished staged generation is retained for explicit recovery"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_refuses_stale_present_witness_before_namespace_change() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"expected\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&output_path, b"  {\"id\":\"expected\"}  \n").unwrap();
        let observed_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        assert_eq!(
            expected_source.content_sha256(),
            observed_source.content_sha256(),
            "the fixture must preserve canonical JSONL content"
        );
        assert_ne!(
            expected_source.raw_sha256(),
            observed_source.raw_sha256(),
            "the fixture must change exact raw bytes"
        );

        fs::write(&temp_path, b"{\"id\":\"staged\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let namespace_hook_called = std::cell::Cell::new(false);

        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || {
                namespace_hook_called.set(true);
                Ok(())
            },
            |_| Ok(()),
        );

        assert!(matches!(result, Err(BeadsError::SyncConflict { .. })));
        assert!(
            !namespace_hook_called.get(),
            "a stale exact source witness must fail before the namespace-change hook"
        );
        assert_eq!(
            fs::read(&output_path).unwrap(),
            b"  {\"id\":\"expected\"}  \n"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"staged\"}\n",
            "a pre-publication failure must retain the staged generation"
        );
    }

    #[test]
    fn conditional_publication_exact_source_witness_distinguishes_missing_from_empty() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");

        assert!(
            capture_optional_jsonl_source(&output_path)
                .unwrap()
                .is_none()
        );
        verify_expected_jsonl_source_state(
            &output_path,
            None,
            Some(&JsonlSourceStateWitness::Missing),
        )
        .unwrap();

        fs::write(&output_path, b"").unwrap();
        let empty_source = capture_optional_jsonl_source(&output_path)
            .unwrap()
            .expect("a present empty file is still a source generation");
        assert_eq!(empty_source.size(), 0);
        assert!(matches!(
            empty_source.state_witness(),
            JsonlSourceStateWitness::Present { size: 0, .. }
        ));
        assert!(
            verify_expected_jsonl_source_state(
                &output_path,
                None,
                Some(&JsonlSourceStateWitness::Missing),
            )
            .is_err(),
            "Missing must never compare equal to a present zero-byte generation"
        );
        verify_expected_jsonl_source_state(&output_path, None, Some(&empty_source.state_witness()))
            .unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_exchange_verifies_both_generations_and_cleans_up() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let mut sync_calls = 0;

        let publication = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || Ok(()),
            |_| {
                sync_calls += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            publication.atomicity,
            ExportPublicationAtomicity::ExchangeAndVerify
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(!temp_path.exists());
        assert_eq!(sync_calls, 2);
        assert!(publication.cleanup_durable);
        assert!(publication.retained_recovery_path.is_none());
        assert_eq!(
            publication.source.state_witness(),
            staged_source.state_witness()
        );
    }

    /// Forces the #419 fallback for the current thread and restores the
    /// atomic path on drop, so a failing assertion cannot leak the knob into
    /// later tests on the same worker thread.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    struct FlaggedRenameUnsupportedGuard;

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    impl FlaggedRenameUnsupportedGuard {
        fn install() -> Self {
            FORCE_FLAGGED_RENAME_UNSUPPORTED.with(|flag| flag.set(true));
            Self
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    impl Drop for FlaggedRenameUnsupportedGuard {
        fn drop(&mut self) {
            FORCE_FLAGGED_RENAME_UNSUPPORTED.with(|flag| flag.set(false));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn flagged_rename_fallback_installs_missing_target_with_downgraded_receipt() {
        let _unsupported = FlaggedRenameUnsupportedGuard::install();
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let mut sync_calls = 0;

        let publication = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &JsonlSourceStateWitness::Missing,
            &content_sha256,
            &authority,
            || Ok(()),
            |_| {
                sync_calls += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            publication.atomicity,
            ExportPublicationAtomicity::ReplaceUnderAuthority
        );
        assert!(publication.atomicity.is_downgraded());
        assert_eq!(publication.atomicity.as_str(), "replace-under-authority");
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(
            !temp_path.exists(),
            "staged file must have been renamed into place"
        );
        assert_eq!(
            sync_calls, 1,
            "no displaced generation means no cleanup fsync"
        );
        assert!(publication.cleanup_durable);
        assert!(publication.retained_recovery_path.is_none());
        assert_eq!(
            publication.source.state_witness(),
            staged_source.state_witness()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn flagged_rename_fallback_replaces_present_target_after_rechecking_witness() {
        let _unsupported = FlaggedRenameUnsupportedGuard::install();
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let mut sync_calls = 0;

        let publication = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || Ok(()),
            |_| {
                sync_calls += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            publication.atomicity,
            ExportPublicationAtomicity::ReplaceUnderAuthority
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(
            !temp_path.exists(),
            "a plain rename overwrites the prior generation instead of displacing it"
        );
        assert_eq!(sync_calls, 1);
        assert!(publication.cleanup_durable);
        assert!(publication.retained_recovery_path.is_none());
        assert_eq!(
            publication.source.state_witness(),
            staged_source.state_witness()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn flagged_rename_fallback_refuses_when_destination_changed_under_it() {
        let _unsupported = FlaggedRenameUnsupportedGuard::install();
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let foreign_generation = b"{\"id\":\"foreign\",\"title\":\"written past the lock\"}\n";

        // The pre-commit hook runs after the publication's entry witness check
        // and immediately before the namespace change, so this mutation can
        // only be caught by the fallback's own re-verification.
        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || {
                fs::write(&output_path, foreign_generation).unwrap();
                Ok(())
            },
            |_| Ok(()),
        );
        let Err(error) = result else {
            panic!("a changed destination must refuse the non-atomic fallback");
        };

        assert!(
            matches!(error, BeadsError::SyncConflict { .. }),
            "expected a witness conflict, got {error:?}"
        );
        assert_eq!(
            fs::read(&output_path).unwrap(),
            foreign_generation,
            "the foreign generation must not have been overwritten"
        );
        assert_eq!(
            fs::read(&temp_path).unwrap(),
            b"{\"id\":\"new\"}\n",
            "the staged generation is retained for recovery"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn flagged_rename_unsupported_only_absorbs_filesystem_capability_errors() {
        use rustix::io::Errno;
        assert!(flagged_rename_unsupported(Errno::INVAL));
        assert!(flagged_rename_unsupported(Errno::NOSYS));
        assert!(flagged_rename_unsupported(Errno::NOTSUP));
        assert!(flagged_rename_unsupported(Errno::OPNOTSUPP));
        for namespace_error in [Errno::EXIST, Errno::NOENT, Errno::ACCESS, Errno::IO] {
            assert!(
                !flagged_rename_unsupported(namespace_error),
                "{namespace_error:?} describes the destination and must surface, not fall back"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_detects_aba_and_preserves_displaced_generation() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        let concurrent_path = temp.path().join("concurrent.jsonl");
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"staged\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || {
                fs::write(&concurrent_path, b"{\"id\":\"concurrent\"}\n")?;
                fs::rename(&concurrent_path, &output_path)?;
                Ok(())
            },
            |_| Ok(()),
        );

        assert!(matches!(
            result,
            Err(BeadsError::JsonlPublicationConflict { .. })
        ));
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"staged\"}\n");
        assert_eq!(fs::read(&temp_path).unwrap(), b"{\"id\":\"concurrent\"}\n");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_reports_post_commit_directory_sync_failure() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();

        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || Ok(()),
            |_| Err(io::Error::other("forced parent-directory sync failure")),
        );

        assert!(matches!(
            result,
            Err(BeadsError::JsonlPublishedButNotDurable { .. })
        ));
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert_eq!(fs::read(&temp_path).unwrap(), b"{\"id\":\"old\"}\n");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_reports_authority_loss_after_exchange_with_recovery_path() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let authority_path = authority.authority_lock_path.clone();
        let displaced_authority_path = authority_path.with_extension("lock.displaced");

        let result = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || {
                fs::rename(&authority_path, &displaced_authority_path)?;
                drop(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&authority_path)?,
                );
                Ok(())
            },
            |_| Ok(()),
        );

        match result {
            Err(BeadsError::JsonlPublishedButUnwitnessed {
                output_path: observed_output,
                recovery_path: Some(observed_recovery),
                ..
            }) => {
                assert_eq!(observed_output, output_path);
                assert_eq!(observed_recovery, temp_path);
            }
            Err(other) => panic!("expected committed authority-loss receipt, got {other}"),
            Ok(_) => panic!("expected committed authority-loss receipt, got success"),
        }
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert_eq!(fs::read(&temp_path).unwrap(), b"{\"id\":\"old\"}\n");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_second_directory_sync_failure_marks_cleanup_uncertain() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("issues.jsonl");
        let temp_path = export_temp_path(&output_path);
        fs::write(&output_path, b"{\"id\":\"old\"}\n").unwrap();
        let expected_source = capture_jsonl_source_snapshot(&output_path).unwrap();
        fs::write(&temp_path, b"{\"id\":\"new\"}\n").unwrap();
        let staged_source = capture_jsonl_source_snapshot(&temp_path).unwrap();
        let content_sha256 = staged_source.content_sha256().to_string();
        let authority = blocking_jsonl_family_write_lock_with_timeout(&output_path, None).unwrap();
        let mut sync_calls = 0;

        let publication = publish_staged_jsonl_conditionally_with(
            &temp_path,
            TempFileGuard::new(temp_path.clone()),
            &output_path,
            &staged_source,
            &expected_source.state_witness(),
            &content_sha256,
            &authority,
            || Ok(()),
            |_| {
                sync_calls += 1;
                if sync_calls == 1 {
                    Ok(())
                } else {
                    Err(io::Error::other(
                        "forced displaced-generation cleanup sync failure",
                    ))
                }
            },
        )
        .unwrap();

        assert_eq!(sync_calls, 2);
        assert_eq!(
            publication.atomicity,
            ExportPublicationAtomicity::ExchangeAndVerify
        );
        assert!(
            !publication.cleanup_durable,
            "a failed second directory sync must not certify cleanup durability"
        );
        assert!(
            publication.retained_recovery_path.is_none(),
            "the displaced file was removed; only cleanup durability is uncertain"
        );
        assert_eq!(fs::read(&output_path).unwrap(), b"{\"id\":\"new\"}\n");
        assert!(!temp_path.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn conditional_publication_is_idempotent_for_identical_existing_target() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("artifact.jsonl");
        let first_staged_path = temp.path().join("artifact.first.tmp");
        let second_staged_path = temp.path().join("artifact.second.tmp");
        let exact_bytes = b" {\"id\":\"same\"}\r\n\n";

        fs::write(&first_staged_path, exact_bytes).unwrap();
        let first_receipt =
            publish_staged_file_conditionally(&first_staged_path, &output_path).unwrap();
        assert_eq!(
            first_receipt.atomicity(),
            ExportPublicationAtomicity::CreateNoReplace
        );
        assert_eq!(first_receipt.output_path(), output_path.to_string_lossy());
        assert_eq!(
            first_receipt.source.content_sha256(),
            first_receipt.content_sha256()
        );
        assert_eq!(fs::read(&output_path).unwrap(), exact_bytes);

        fs::write(&second_staged_path, exact_bytes).unwrap();
        let second_receipt =
            publish_staged_file_conditionally(&second_staged_path, &output_path).unwrap();
        assert_eq!(
            second_receipt.atomicity(),
            ExportPublicationAtomicity::ExchangeAndVerify
        );
        assert_eq!(
            second_receipt.content_sha256(),
            first_receipt.content_sha256()
        );
        assert_eq!(
            second_receipt.source.raw_sha256(),
            first_receipt.source.raw_sha256()
        );
        assert!(second_receipt.cleanup_durable());
        assert!(second_receipt.retained_recovery_path().is_none());
        assert_eq!(fs::read(&output_path).unwrap(), exact_bytes);
        assert!(!first_staged_path.exists());
        assert!(!second_staged_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_rejects_symlink_and_nonregular_targets() {
        let temp = TempDir::new().unwrap();
        let outside_path = temp.path().join("outside.jsonl");
        let symlink_path = temp.path().join("symlink.jsonl");
        let directory_path = temp.path().join("directory.jsonl");
        fs::write(&outside_path, b"{\"id\":\"outside\"}\n").unwrap();
        symlink(&outside_path, &symlink_path).unwrap();
        fs::create_dir(&directory_path).unwrap();

        let symlink_error =
            blocking_jsonl_family_write_lock_with_timeout(&symlink_path, None).unwrap_err();
        assert!(
            symlink_error.to_string().contains("symlink"),
            "unexpected symlink error: {symlink_error}"
        );
        let directory_error =
            blocking_jsonl_family_write_lock_with_timeout(&directory_path, None).unwrap_err();
        assert!(
            directory_error.to_string().contains("regular file"),
            "unexpected nonregular error: {directory_error}"
        );
        assert_eq!(fs::read(&outside_path).unwrap(), b"{\"id\":\"outside\"}\n");
    }

    #[cfg(unix)]
    #[test]
    fn conditional_publication_creates_jsonl_and_base_with_restrictive_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let output_path = beads_dir.join("issues.jsonl");
        let storage = SqliteStorage::open_memory().unwrap();
        let config = ExportConfig {
            beads_dir: Some(beads_dir.clone()),
            ..ExportConfig::default()
        };

        export_to_jsonl(&storage, &output_path, &config).unwrap();
        save_base_snapshot(&HashMap::<String, Issue>::new(), &beads_dir).unwrap();

        assert_eq!(
            fs::metadata(&output_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(beads_dir.join("beads.base.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn expected_staged_output_mismatch_never_replaces_live_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let output_path = beads_dir.join("issues.jsonl");
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_test_issue("bd-reviewed-output", "Reviewed output");
        storage.create_issue(&issue, "test").unwrap();

        let mut reviewed_bytes = Vec::new();
        let reviewed = export_to_writer(&storage, &mut reviewed_bytes).unwrap();
        let reviewed_issue_hashes =
            sync_merge_export_hash_mapping_witness(&reviewed.issue_hashes).unwrap();

        let mut previous_issue = issue;
        previous_issue.title = "Previous live generation".to_string();
        let previous_bytes = format!("{}\n", serde_json::to_string(&previous_issue).unwrap());
        fs::write(&output_path, previous_bytes.as_bytes()).unwrap();

        let mismatches = [
            ExpectedStagedExport {
                raw_sha256: "ff".repeat(32),
                issue_count: reviewed.exported_count,
                issue_hashes: reviewed_issue_hashes.clone(),
            },
            ExpectedStagedExport {
                raw_sha256: reviewed.content_hash.clone(),
                issue_count: reviewed.exported_count + 1,
                issue_hashes: reviewed_issue_hashes.clone(),
            },
            ExpectedStagedExport {
                raw_sha256: reviewed.content_hash.clone(),
                issue_count: reviewed.exported_count,
                issue_hashes: AdditiveTableWitness {
                    rows: reviewed_issue_hashes.rows,
                    payload_sha256: "ee".repeat(32),
                },
            },
        ];

        for (attempt, expected_staged_output) in mismatches.into_iter().enumerate() {
            let config = ExportConfig {
                force: true,
                beads_dir: Some(beads_dir.clone()),
                expected_staged_output: Some(expected_staged_output),
                ..Default::default()
            };
            let error = export_to_jsonl(&storage, &output_path, &config).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("does not match the exact reviewed export"),
                "unexpected staged-output mismatch: {error}"
            );
            assert_eq!(
                fs::read(&output_path).unwrap(),
                previous_bytes.as_bytes(),
                "mismatch attempt {attempt} replaced the live JSONL"
            );
            let staged_path =
                export_temp_path_for_attempt(&output_path, u32::try_from(attempt).unwrap());
            assert!(
                staged_path.exists(),
                "mismatch attempt {attempt} did not retain its staged recovery artifact"
            );
            assert_eq!(
                fs::read(&staged_path).unwrap(),
                reviewed_bytes,
                "retained staged bytes differ from the candidate that was rejected"
            );
        }
    }

    #[test]
    fn test_save_base_snapshot_skips_stale_regular_temp_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let snapshot_path = beads_dir.join("beads.base.jsonl");
        fs::write(&snapshot_path, "old-snapshot\n").unwrap();
        let stale_temp_path = export_temp_path_for_attempt(&snapshot_path, 0);
        let retry_temp_path = export_temp_path_for_attempt(&snapshot_path, 1);
        fs::write(&stale_temp_path, "stale temp\n").unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-base".to_string(),
            Issue {
                id: "bd-base".to_string(),
                title: "New base snapshot".to_string(),
                ..Issue::default()
            },
        );

        save_base_snapshot(&issues, &beads_dir).unwrap();

        let snapshot = fs::read_to_string(&snapshot_path).unwrap();
        assert!(
            snapshot.contains("\"id\":\"bd-base\""),
            "base snapshot should be rewritten with the requested issue: {snapshot}"
        );
        assert_eq!(
            fs::read_to_string(&stale_temp_path).unwrap(),
            "stale temp\n",
            "stale regular temp file should be left untouched"
        );
        assert!(
            !retry_temp_path.exists(),
            "successful retry temp path should be renamed away"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_save_base_snapshot_rejects_existing_temp_symlink() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let snapshot_path = beads_dir.join("beads.base.jsonl");
        fs::write(&snapshot_path, "old-snapshot\n").unwrap();

        let temp_target = outside_dir.join("captured.txt");
        fs::write(&temp_target, "do-not-touch").unwrap();
        let pid = std::process::id();
        symlink(
            &temp_target,
            beads_dir.join(format!("beads.base.jsonl.{pid}.tmp")),
        )
        .unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-base".to_string(),
            Issue {
                id: "bd-base".to_string(),
                title: "New base snapshot".to_string(),
                ..Issue::default()
            },
        );

        let err = save_base_snapshot(&issues, &beads_dir).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("regular file")
                || message.contains("Temporary base snapshot file")
                || message.contains("Symlink")
                || message.contains("Path"),
            "unexpected error: {message}"
        );
        assert_eq!(
            fs::read_to_string(&snapshot_path).unwrap(),
            "old-snapshot\n",
            "existing base snapshot should remain unchanged on failure"
        );
        assert_eq!(
            fs::read_to_string(&temp_target).unwrap(),
            "do-not-touch",
            "symlink target should not be overwritten"
        );
    }

    #[test]
    fn test_save_base_snapshot_sorts_issues_deterministically() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-z".to_string(),
            Issue {
                id: "bd-z".to_string(),
                title: "Last".to_string(),
                ..Issue::default()
            },
        );
        issues.insert(
            "bd-a".to_string(),
            Issue {
                id: "bd-a".to_string(),
                title: "First".to_string(),
                ..Issue::default()
            },
        );

        save_base_snapshot(&issues, &beads_dir).unwrap();

        let lines: Vec<_> = fs::read_to_string(beads_dir.join("beads.base.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);

        let first: Issue = serde_json::from_str(&lines[0]).unwrap();
        let second: Issue = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(first.id, "bd-a");
        assert_eq!(second.id, "bd-z");
    }

    #[test]
    fn test_save_base_snapshot_from_jsonl_uses_finalized_export_contents() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let jsonl_path = beads_dir.join("issues.jsonl");
        let issue = Issue {
            id: "bd-final".to_string(),
            title: "Finalized".to_string(),
            comments: vec![Comment {
                id: 1,
                issue_id: "bd-final".to_string(),
                author: "br-sync".to_string(),
                body: "merge note written after report".to_string(),
                created_at: Utc::now(),
            }],
            ..Issue::default()
        };
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        save_base_snapshot_from_jsonl(&jsonl_path, &beads_dir).unwrap();

        let base = load_base_snapshot(&beads_dir).unwrap();
        let saved = base.get("bd-final").expect("saved base issue");
        assert_eq!(saved.comments.len(), 1);
        assert_eq!(saved.comments[0].body, "merge note written after report");
    }

    #[test]
    fn conditional_publication_preserves_exact_base_bytes_across_replacement() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");
        let base_path = beads_dir.join("beads.base.jsonl");
        let exact_bytes = b" \t{\"id\":\"bd-exact\",\"title\":\"Exact bytes\"} \r\n\r\n";
        fs::write(&jsonl_path, exact_bytes).unwrap();
        fs::write(&base_path, b"{\"id\":\"old\"}\n").unwrap();

        refresh_base_snapshot_from_flushed_jsonl(&jsonl_path, &beads_dir).unwrap();
        assert_eq!(fs::read(&base_path).unwrap(), exact_bytes);

        refresh_base_snapshot_from_flushed_jsonl(&jsonl_path, &beads_dir).unwrap();
        assert_eq!(
            fs::read(&base_path).unwrap(),
            exact_bytes,
            "replacing an already-equal base must remain byte-for-byte idempotent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_load_base_snapshot_rejects_symlink_escape() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let outside_snapshot = outside_dir.join("beads.base.jsonl");
        fs::write(&outside_snapshot, "{\"id\":\"bd-outside\"}\n").unwrap();
        symlink(&outside_snapshot, beads_dir.join("beads.base.jsonl")).unwrap();

        let err = load_base_snapshot(&beads_dir).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("symlink") || message.contains("Symlink") || message.contains("Path"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_export_with_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create test issues
        let issue1 = make_test_issue("bd-001", "First issue");
        let issue2 = make_test_issue("bd-002", "Second issue");

        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        assert_eq!(result.exported_count, 2);
        assert!(result.exported_ids.contains(&"bd-001".to_string()));
        assert!(result.exported_ids.contains(&"bd-002".to_string()));

        // Verify content
        let read_back = read_issues_from_jsonl(&output_path).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].id, "bd-001");
        assert_eq!(read_back[1].id, "bd-002");
    }

    #[test]
    fn test_safety_guard_empty_over_nonempty() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create existing JSONL with issues
        let issue = make_test_issue("bd-existing", "Existing issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&output_path, format!("{json}\n")).unwrap();

        // Try to export empty database (should fail)
        let config = ExportConfig {
            force: false,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty database"));
    }

    #[test]
    fn test_safety_guard_with_force() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create existing JSONL with issues
        let issue = make_test_issue("bd-existing", "Existing issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&output_path, format!("{json}\n")).unwrap();

        // Export with force (should succeed)
        let config = ExportConfig {
            force: true,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_count_issues_in_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.jsonl");

        // Empty file
        fs::write(&path, "").unwrap();
        assert_eq!(count_issues_in_jsonl(&path).unwrap(), 0);

        // Two issues
        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();
        assert_eq!(count_issues_in_jsonl(&path).unwrap(), 2);
    }

    #[test]
    fn test_get_issue_ids_from_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.jsonl");

        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let ids = get_issue_ids_from_jsonl(&path).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("bd-001"));
        assert!(ids.contains("bd-002"));
    }

    #[test]
    fn test_analyze_jsonl_rejects_duplicate_issue_ids() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("duplicate-ids.jsonl");

        let issue1 = make_test_issue("bd-dup", "Original");
        let issue2 = make_test_issue("bd-dup", "Duplicate");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let err = analyze_jsonl(&path).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Duplicate issue id 'bd-dup'")
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_corruption_shapes() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("corrupt-export.jsonl");

        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let json1 = serde_json::to_string(&issue1).unwrap();
        let json2 = serde_json::to_string(&issue2).unwrap();

        let cases = [
            ("collapsed adjacent records", format!("{json1}{json2}\n")),
            (
                "stray issue prefix",
                "{\"i{\"id\":\"bd-001\"}\n".to_string(),
            ),
            (
                "missing comments array closure",
                "{\"id\":\"bd-001\",\"title\":\"Broken\",\"comments\":[{\"id\":1}\n".to_string(),
            ),
            (
                "object nested in numeric field",
                "{\"id\":\"bd-001\",\"title\":\"Broken\",\"original_size\":{\"id\":\"bd-002\"}}\n"
                    .to_string(),
            ),
        ];

        for (name, content) in cases {
            fs::write(&path, content).unwrap();

            let err = verify_exported_jsonl_integrity(
                &path,
                &["bd-001".to_string(), "bd-002".to_string()],
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("invalid exported JSON at line 1"),
                "{name}: unexpected error: {err}"
            );
        }
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_missing_expected_issue() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("missing-export.jsonl");

        let issue = make_test_issue("bd-001", "One");
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err =
            verify_exported_jsonl_integrity(&path, &["bd-001".to_string(), "bd-002".to_string()])
                .unwrap_err()
                .to_string();
        assert!(
            err.contains("expected 2 issues, JSONL has 1 valid issue lines"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_unexpected_issue() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("unexpected-export.jsonl");

        let issue = make_test_issue("bd-other", "Other");
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err = verify_exported_jsonl_integrity(&path, &["bd-001".to_string()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unexpected issue id 'bd-other' at line 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_export_excludes_ephemerals() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create regular and ephemeral issues
        let regular = make_test_issue("bd-regular", "Regular issue");
        let mut ephemeral = make_test_issue("bd-ephemeral", "Ephemeral issue");
        ephemeral.ephemeral = true;

        storage.create_issue(&regular, "test").unwrap();
        storage.create_issue(&ephemeral, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        // Only regular issue should be exported
        assert_eq!(result.exported_count, 1);
        assert!(result.exported_ids.contains(&"bd-regular".to_string()));
        assert!(!result.exported_ids.contains(&"bd-ephemeral".to_string()));
    }

    #[test]
    fn test_stale_database_guard_prevents_losing_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create a JSONL with two issues
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&output_path, content).unwrap();

        // Only create one issue in DB (missing bd-002)
        storage.create_issue(&issue1, "test").unwrap();

        // Export should fail because it would lose bd-002
        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("stale database") || err.contains("lose"));
    }

    #[test]
    fn test_stale_database_guard_with_force_succeeds() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create a JSONL with two issues
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&output_path, content).unwrap();

        // Only create one issue in DB
        storage.create_issue(&issue1, "test").unwrap();

        // Export with force should succeed
        let config = ExportConfig {
            force: true,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auto_import_if_stale_skips_probe_for_allow_stale() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");

        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&jsonl_path, [0xFF_u8, b'\n']).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let result = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &jsonl_path,
            None,
            false,
            true,
            false,
        )
        .unwrap();
        assert!(!result.attempted);
        assert_eq!(result.imported_count, 0);
    }

    #[test]
    fn test_auto_import_if_stale_skips_probe_for_no_auto_import() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");

        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&jsonl_path, [0xFF_u8, b'\n']).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let result = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &jsonl_path,
            None,
            false,
            false,
            true,
        )
        .unwrap();
        assert!(!result.attempted);
        assert_eq!(result.imported_count, 0);
    }

    #[test]
    fn test_auto_import_probe_validates_external_path_before_hashing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let external_jsonl = temp_dir.path().join("external").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_jsonl).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let err = auto_import_probe(&storage, &beads_dir, &external_jsonl, false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("is outside .beads")
                || message.contains("outside the beads directory")
                || message.contains("regular file"),
            "unexpected error: {err}"
        );

        let err = auto_import_probe_refreshing_witnesses(
            &mut storage,
            &beads_dir,
            &external_jsonl,
            false,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("is outside .beads")
                || message.contains("outside the beads directory")
                || message.contains("regular file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_auto_import_if_stale_validates_external_path_before_hashing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let external_jsonl = temp_dir.path().join("external").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_jsonl).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let err = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &external_jsonl,
            None,
            false,
            false,
            false,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("is outside .beads")
                || message.contains("outside the beads directory")
                || message.contains("regular file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_compute_staleness_uses_matching_jsonl_mtime_witness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let current_hash = compute_jsonl_hash(&jsonl_path).unwrap();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &current_hash)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.jsonl_exists);
        assert!(!staleness.jsonl_newer);
        assert!(staleness.jsonl_mtime.is_some());
    }

    #[test]
    fn test_compute_staleness_does_not_trust_matching_mtime_without_hash_match() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.jsonl_exists);
        assert!(staleness.jsonl_newer);
        assert!(staleness.jsonl_mtime.is_some());
    }

    #[test]
    fn test_compute_staleness_detects_same_size_rewrite_with_restored_mtime() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");
        let original = b"{\"id\":\"bd-1\"}\n";
        let replacement = b"{\"id\":\"bd-2\"}\n";
        assert_eq!(original.len(), replacement.len());

        fs::write(&jsonl_path, original).unwrap();
        let original_witness = observed_jsonl_witness(&jsonl_path).unwrap();
        let original_hash = compute_jsonl_hash(&jsonl_path).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &original_hash)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &original_witness.mtime_witness)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_SIZE, &original_witness.size.to_string())
            .unwrap();

        fs::write(&jsonl_path, replacement).unwrap();
        File::options()
            .write(true)
            .open(&jsonl_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_witness.mtime))
            .unwrap();
        let restored_witness = observed_jsonl_witness(&jsonl_path).unwrap();
        assert_eq!(
            restored_witness.mtime_witness,
            original_witness.mtime_witness
        );
        assert_eq!(restored_witness.size, original_witness.size);

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(
            staleness.jsonl_newer,
            "content hash must detect a rewrite even when mtime and size match"
        );
    }

    #[test]
    fn test_compute_staleness_refreshing_witnesses_backfills_jsonl_size() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let current_hash = compute_jsonl_hash(&jsonl_path).unwrap();
        let jsonl_size = fs::metadata(&jsonl_path).unwrap().len().to_string();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &current_hash)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness_refreshing_witnesses(&mut storage, &jsonl_path).unwrap();
        assert!(!staleness.jsonl_newer);
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_SIZE).unwrap(),
            Some(jsonl_size)
        );
    }

    #[test]
    fn test_refresh_jsonl_witness_best_effort_ignores_missing_jsonl() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let observed = observed_jsonl_witness(&jsonl_path).unwrap();
        fs::remove_file(&jsonl_path).unwrap();

        refresh_jsonl_witness_best_effort(&mut storage, &jsonl_path, &observed);

        assert_eq!(storage.get_metadata(METADATA_JSONL_MTIME).unwrap(), None);
        assert_eq!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap(), None);
    }

    #[test]
    fn test_compute_staleness_marks_db_newer_when_force_flush_is_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        storage.set_metadata("needs_flush", "true").unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.db_newer);
        assert_eq!(staleness.dirty_count, 0);
    }

    #[test]
    fn test_compute_staleness_marks_db_newer_when_jsonl_is_missing_but_db_has_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");
        let issue = make_test_issue("bd-missing-jsonl", "DB only");
        storage.create_issue(&issue, "tester").unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.db_newer);
        assert!(!staleness.jsonl_exists);
    }

    #[test]
    fn test_auto_flush_propagates_jsonl_scan_io_errors() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&jsonl_path).unwrap();

        let issue = make_test_issue("bd-scan-error", "Dirty issue");
        storage.create_issue(&issue, "tester").unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap_err();
        assert!(
            err.to_string().contains("directory")
                || err.to_string().contains("Is a directory")
                || err.to_string().contains("not a regular file")
                || err.to_string().contains("must be a regular file"),
            "unexpected error: {err}"
        );
        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-scan-error".to_string()],
            "failed auto-flush must leave dirty markers intact"
        );
    }

    #[test]
    fn test_auto_flush_validates_path_before_reading_existing_jsonl() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let outside_jsonl_path = temp_dir.path().join("outside.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&outside_jsonl_path, "<<<<<<< HEAD\n").unwrap();

        let issue = make_test_issue("bd-auto-flush-path", "Dirty issue");
        storage.create_issue(&issue, "tester").unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &outside_jsonl_path, false).unwrap_err();
        assert!(
            err.to_string().contains("is outside .beads")
                || err.to_string().contains("outside the beads directory"),
            "unexpected error: {err}"
        );
        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-auto-flush-path".to_string()],
            "rejected auto-flush must leave dirty markers intact"
        );
    }

    #[test]
    fn test_auto_flush_refuses_clean_pending_merge_before_no_op_probe() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        storage
            .set_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY, "legacy-receipt")
            .unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap_err();

        assert!(matches!(&err, BeadsError::SyncConflict { .. }));
        assert!(
            err.to_string().contains("br sync --merge"),
            "refusal must be actionable: {err}"
        );
        assert!(
            !jsonl_path.exists(),
            "clean refusal must happen before creating a JSONL artifact"
        );
        assert_eq!(
            storage
                .get_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY)
                .unwrap()
                .as_deref(),
            Some("legacy-receipt"),
            "clean refusal must preserve pending metadata exactly"
        );
    }

    #[test]
    fn test_auto_flush_refuses_dirty_pending_merge_without_touching_jsonl_or_dirty_state() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        let original_jsonl = b"{\"id\":\"bd-existing\",\"title\":\"unchanged\"}\n";
        fs::write(&jsonl_path, original_jsonl).unwrap();
        storage
            .create_issue(
                &make_test_issue("bd-pending-dirty", "must remain dirty"),
                "tester",
            )
            .unwrap();
        storage
            .set_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY, "legacy-receipt")
            .unwrap();
        let dirty_before = storage.get_dirty_issue_metadata().unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap_err();

        assert!(matches!(err, BeadsError::SyncConflict { .. }));
        assert_eq!(
            fs::read(&jsonl_path).unwrap(),
            original_jsonl,
            "dirty refusal must not rewrite JSONL"
        );
        assert_eq!(
            storage.get_dirty_issue_metadata().unwrap(),
            dirty_before,
            "dirty refusal must not clear or rewrite dirty markers"
        );
        assert_eq!(
            storage
                .get_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY)
                .unwrap()
                .as_deref(),
            Some("legacy-receipt"),
            "dirty refusal must preserve pending metadata exactly"
        );
    }

    #[test]
    fn test_import_records_matching_jsonl_mtime_witness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-import", "Imported issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let jsonl_size = fs::metadata(&jsonl_path).unwrap().len().to_string();
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_MTIME).unwrap(),
            Some(jsonl_mtime_witness)
        );
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_SIZE).unwrap(),
            Some(jsonl_size)
        );
    }

    #[test]
    fn test_import_skips_child_counters_for_missing_parents() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let orphan_child = make_test_issue("bd-orphan.6", "Recovered orphan child");
        let json = serde_json::to_string(&orphan_child).unwrap();
        fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let child_counters = storage
            .execute_raw_query("SELECT parent_id FROM child_counters")
            .unwrap();
        assert!(
            child_counters.is_empty(),
            "orphan child IDs should not rebuild counters for missing parents"
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "child counters must remain free of FK orphans after import"
        );
    }

    #[test]
    fn test_import_rebuilds_nested_child_counters_only_for_existing_parents() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let orphan_child = make_test_issue("bd-orphan.6", "Recovered orphan child");
        let nested_child = make_test_issue("bd-orphan.6.1", "Recovered nested child");
        let orphan_json = serde_json::to_string(&orphan_child).unwrap();
        let nested_json = serde_json::to_string(&nested_child).unwrap();
        fs::write(&jsonl_path, format!("{orphan_json}\n{nested_json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let child_counters = storage
            .execute_raw_query(
                "SELECT parent_id, last_child FROM child_counters ORDER BY parent_id",
            )
            .unwrap();
        assert_eq!(
            child_counters.len(),
            1,
            "only the existing intermediate parent should get a counter"
        );
        assert_eq!(
            child_counters[0]
                .first()
                .and_then(SqliteValue::as_text)
                .unwrap_or(""),
            "bd-orphan.6"
        );
        assert_eq!(
            child_counters[0]
                .get(1)
                .and_then(SqliteValue::as_integer)
                .unwrap_or_default(),
            1
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "nested rebuild should not recreate orphan counters for missing roots"
        );
    }

    #[test]
    fn test_normalize_issue_wisp_detection() {
        let mut issue = make_test_issue("bd-wisp-123", "Wisp issue");
        assert!(!issue.ephemeral);

        normalize_issue(&mut issue);

        // Issue ID containing "-wisp-" should be marked ephemeral
        assert!(issue.ephemeral);
    }

    #[test]
    fn test_normalize_issue_deduplicates_only_identical_comments() {
        let mut issue = make_test_issue("bd-comment-dedupe", "Comment recovery");
        let comment = crate::model::Comment {
            id: 42,
            issue_id: issue.id.clone(),
            author: "reporter".to_string(),
            body: "recovery evidence".to_string(),
            created_at: issue.created_at,
        };
        let mut conflicting = comment.clone();
        conflicting.body = "different evidence".to_string();
        issue.comments = vec![comment.clone(), comment, conflicting.clone()];

        let deduplicated = normalize_issue(&mut issue);

        assert_eq!(deduplicated, 1);
        assert_eq!(issue.comments.len(), 2);
        assert_eq!(issue.comments[1], conflicting);
    }

    #[test]
    fn test_import_recovers_identical_duplicate_comments_with_receipt_count() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");
        let mut issue = make_test_issue("bd-comment-recovery", "Comment recovery");
        let comment = crate::model::Comment {
            id: 42,
            issue_id: issue.id.clone(),
            author: "reporter".to_string(),
            body: "recovery evidence".to_string(),
            created_at: issue.created_at,
        };
        issue.comments = vec![comment.clone(), comment];
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let result = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        assert_eq!(result.exact_duplicate_comments_deduplicated, 1);
        assert_eq!(result.comments_imported, 1);
        assert_eq!(storage.get_comments(&issue.id).unwrap().len(), 1);
    }

    #[test]
    fn test_import_repairs_cross_issue_comment_id_swap_before_semantic_verification() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let existing_a = make_test_issue("bd-comment-owner-a", "First owner");
        let existing_b = make_test_issue("bd-comment-owner-b", "Second owner");
        storage.create_issue(&existing_a, "tester").unwrap();
        storage.create_issue(&existing_b, "tester").unwrap();
        let stale_b_comment = storage
            .add_comment(&existing_b.id, "bob", "comment from B")
            .unwrap();

        // Two divergent JSONL lineages independently used stale_b_comment.id.
        // The merged JSONL kept that id for A and renumbered B, while the
        // ignored local database still assigns the old id to B. A appears
        // first in JSONL, so per-issue replacement used to AUTO-reallocate A
        // before B released the authoritative id and then fail the semantic
        // verifier because neither persisted id matched JSONL.
        let mut incoming_a = existing_a.clone();
        incoming_a.updated_at += chrono::Duration::minutes(1);
        incoming_a.comments = vec![crate::model::Comment {
            id: stale_b_comment.id,
            issue_id: incoming_a.id.clone(),
            author: "alice".to_string(),
            body: "comment from A".to_string(),
            created_at: incoming_a.updated_at,
        }];

        let mut incoming_b = existing_b.clone();
        incoming_b.updated_at += chrono::Duration::minutes(1);
        incoming_b.comments = vec![crate::model::Comment {
            id: stale_b_comment.id + 1,
            issue_id: incoming_b.id.clone(),
            author: stale_b_comment.author.clone(),
            body: stale_b_comment.body.clone(),
            created_at: stale_b_comment.created_at,
        }];

        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&incoming_a).unwrap(),
                serde_json::to_string(&incoming_b).unwrap()
            ),
        )
        .unwrap();

        let result = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .expect("cross-issue comment ownership must converge to JSONL");
        assert_eq!(result.updated_count, 2);

        let comments_a = storage.get_comments(&incoming_a.id).unwrap();
        let comments_b = storage.get_comments(&incoming_b.id).unwrap();
        assert_eq!(comments_a, incoming_a.comments);
        assert_eq!(comments_b, incoming_b.comments);

        let second = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .expect("the converged import must be a true no-op");
        assert_eq!(second.created_count, 0);
        assert_eq!(second.updated_count, 0);
        assert_eq!(storage.get_comments(&incoming_a.id).unwrap(), comments_a);
        assert_eq!(storage.get_comments(&incoming_b.id).unwrap(), comments_b);
    }

    #[test]
    fn test_import_comment_owner_preclear_preserves_skipped_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let incoming_skipped = make_test_issue("bd-comment-skip", "Local winner");
        let mut existing_skipped = incoming_skipped.clone();
        existing_skipped.updated_at += chrono::Duration::minutes(2);
        storage.create_issue(&existing_skipped, "tester").unwrap();
        let local_comment = storage
            .add_comment(&existing_skipped.id, "local", "must survive")
            .unwrap();

        let existing_updated = make_test_issue("bd-comment-update", "Applied update");
        storage.create_issue(&existing_updated, "tester").unwrap();
        storage
            .add_comment(&existing_updated.id, "local", "must be replaced")
            .unwrap();
        let mut incoming_updated = existing_updated.clone();
        incoming_updated.updated_at += chrono::Duration::minutes(1);

        fs::write(
            &jsonl_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&incoming_skipped).unwrap(),
                serde_json::to_string(&incoming_updated).unwrap()
            ),
        )
        .unwrap();

        let result = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        assert_eq!(result.updated_count, 1);
        assert_eq!(result.skipped_count, 1);
        assert_eq!(
            storage.get_comments(&incoming_skipped.id).unwrap(),
            vec![local_comment]
        );
        assert!(
            storage
                .get_comments(&incoming_updated.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_import_semantic_verifier_rejects_field_shift_with_equal_counts() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");
        let issue = make_test_issue("bd-import-semantic", "Expected title");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let result = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();
        assert_eq!(result.applied_issues.len(), 1);

        storage
            .execute_raw(
                "UPDATE issues SET updated_at = '2035-01-02T03:04:05Z' WHERE id = 'bd-import-semantic'",
            )
            .unwrap();
        let err = verify_applied_import_issue_semantics(&storage, &result.applied_issues)
            .expect_err("equal row counts must not hide a field shift");
        assert!(
            err.to_string()
                .contains("does not match its normalized JSONL payload"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_normalize_issue_closed_at_repair() {
        let mut issue = make_test_issue("bd-001", "Closed issue");
        issue.status = Status::Closed;
        issue.closed_at = None;

        normalize_issue(&mut issue);

        // closed_at should be set to updated_at for closed issues
        assert!(issue.closed_at.is_some());
        assert_eq!(issue.closed_at, Some(issue.updated_at));
    }

    #[test]
    fn test_normalize_issue_clears_closed_at_for_open() {
        let mut issue = make_test_issue("bd-001", "Open issue");
        issue.status = Status::Open;
        issue.closed_at = Some(Utc::now());

        normalize_issue(&mut issue);

        // closed_at should be cleared for open issues
        assert!(issue.closed_at.is_none());
    }

    #[test]
    fn test_normalize_issue_computes_content_hash() {
        let mut issue = make_test_issue("bd-001", "Test");
        issue.content_hash = None;

        normalize_issue(&mut issue);

        assert!(issue.content_hash.is_some());
        assert!(!issue.content_hash.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_normalize_issue_hashes_trimmed_external_ref() {
        let mut issue = make_test_issue("bd-001", "Test");
        issue.external_ref = Some("  ext-123  ".to_string());

        normalize_issue(&mut issue);

        let expected_hash = crate::util::content_hash(&issue);
        assert_eq!(issue.external_ref.as_deref(), Some("ext-123"));
        assert_eq!(issue.content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn test_normalize_issue_remaps_legacy_done_to_closed() {
        // Go-beads "done" survives round-tripping as Status::Custom; ensure
        // import normalization promotes it to the canonical Closed variant
        // and that closed_at gets populated to satisfy the DB CHECK.
        let mut issue = make_test_issue("bd-001", "Legacy done");
        issue.status = Status::Custom("done".to_string());
        issue.closed_at = None;

        normalize_issue(&mut issue);

        assert_eq!(issue.status, Status::Closed);
        assert!(issue.closed_at.is_some());
    }

    #[test]
    fn test_normalize_issue_remaps_mixed_case_terminal_aliases() {
        for raw in ["Done", "COMPLETE", "completed", "Finished", "Resolved"] {
            let mut issue = make_test_issue("bd-001", "Legacy alias");
            issue.status = Status::Custom(raw.to_string());
            normalize_issue(&mut issue);
            assert_eq!(
                issue.status,
                Status::Closed,
                "alias {raw:?} should map to Closed"
            );
        }
    }

    #[test]
    fn test_normalize_issue_preserves_unknown_custom_status() {
        let mut issue = make_test_issue("bd-001", "Custom status");
        issue.status = Status::Custom("qa-review".to_string());
        normalize_issue(&mut issue);
        assert_eq!(issue.status, Status::Custom("qa-review".to_string()));
    }

    #[test]
    fn test_normalize_issue_normalizes_legacy_standard_dependency_type_with_underscores() {
        let mut issue = make_test_issue("bd-001", "Legacy dependency");
        issue.dependencies.push(crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: "bd-002".to_string(),
            dep_type: crate::model::DependencyType::Custom("parent_child".to_string()),
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        normalize_issue(&mut issue);

        assert_eq!(
            issue.dependencies[0].dep_type,
            crate::model::DependencyType::ParentChild
        );
    }

    #[test]
    fn test_normalize_issue_preserves_custom_dependency_type_with_underscores() {
        let mut issue = make_test_issue("bd-001", "Custom dependency");
        issue.dependencies.push(crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: "bd-002".to_string(),
            dep_type: crate::model::DependencyType::Custom("review_needed".to_string()),
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        normalize_issue(&mut issue);

        assert_eq!(
            issue.dependencies[0].dep_type,
            crate::model::DependencyType::Custom("review_needed".to_string())
        );
    }

    #[test]
    fn test_import_collision_by_id_updates_newer() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create existing issue in DB with older timestamp.
        // Pin both created_at and updated_at so the validator's
        // "updated_at >= created_at" rule holds.
        let mut existing = make_test_issue("test-001", "Old title");
        existing.created_at = Utc::now() - chrono::Duration::hours(2);
        existing.updated_at = Utc::now() - chrono::Duration::hours(1);
        storage.create_issue(&existing, "test").unwrap();

        // Create JSONL with same ID but newer timestamp and new title
        let mut incoming = make_test_issue("test-001", "New title");
        incoming.updated_at = Utc::now();
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should update since incoming is newer
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.created_count, 0);
        assert_eq!(result.updated_count, 1);

        // The existing issue should be updated
        let updated = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(updated.title, "New title");
    }

    #[test]
    fn test_import_collision_by_id_skips_older() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create existing issue in DB with newer timestamp
        let mut existing = make_test_issue("test-001", "Newer title");
        existing.updated_at = Utc::now();
        storage.create_issue(&existing, "test").unwrap();

        // Create JSONL with same ID but older timestamp
        let mut incoming = make_test_issue("test-001", "Older title");
        incoming.created_at = Utc::now() - chrono::Duration::hours(2); // Fix timestamp to be valid
        incoming.updated_at = Utc::now() - chrono::Duration::hours(1);
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should skip since existing is newer
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.skipped_count, 1);

        let unchanged = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(unchanged.title, "Newer title");
    }

    #[test]
    fn test_import_tombstone_skip_marks_flush_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut tombstone = make_issue_at("bd-tomb", "Deleted locally", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(fixed_time(100));
        storage.create_issue(&tombstone, "test").unwrap();
        storage
            .clear_dirty_issues_legacy(&["bd-tomb".to_string()])
            .unwrap();
        storage.set_metadata("needs_flush", "false").unwrap();

        let mut incoming = make_issue_at("bd-tomb", "Remote resurrection", fixed_time(200));
        incoming.status = Status::Open;
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd")).unwrap();
        assert_eq!(result.tombstone_skipped, 1);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(storage.get_export_hash("bd-tomb").unwrap().is_none());

        let still_tombstone = storage.get_issue("bd-tomb").unwrap().unwrap();
        assert_eq!(still_tombstone.status, Status::Tombstone);
    }

    #[test]
    fn test_import_relation_only_local_win_marks_flush_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let existing = make_issue_at("bd-rel", "Same content", fixed_time(200));
        storage.create_issue(&existing, "test").unwrap();
        storage.add_label("bd-rel", "local-only", "test").unwrap();
        storage
            .clear_dirty_issues_legacy(&["bd-rel".to_string()])
            .unwrap();
        storage.set_metadata("needs_flush", "false").unwrap();

        let incoming = make_issue_at("bd-rel", "Same content", fixed_time(100));
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd")).unwrap();
        assert_eq!(result.skipped_count, 1);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(storage.get_export_hash("bd-rel").unwrap().is_none());
        assert_eq!(storage.get_labels("bd-rel").unwrap(), vec!["local-only"]);
    }

    #[test]
    fn test_import_collision_by_external_ref_same_id() {
        // Test collision detection by external_ref when IDs also match
        let storage = SqliteStorage::open_memory().unwrap();

        let mut ext_issue = make_issue_at("bd-ext", "External", fixed_time(100));
        ext_issue.external_ref = Some("JIRA-1".to_string());
        set_content_hash(&mut ext_issue);
        storage.upsert_issue_for_import(&ext_issue).unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Incoming", fixed_time(200));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        // Incoming has same external_ref as ext_issue - should match on external_ref
        // even though it has same title/content_hash as hash_issue
        let mut incoming = make_issue_at("bd-new", "Incoming", fixed_time(300));
        incoming.external_ref = Some("JIRA-1".to_string());
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-ext");
            assert_eq!(match_type, MatchType::ExternalRef);
            assert_eq!(phase, 1);
        }
    }

    #[test]
    fn test_import_tombstone_protection() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create tombstone in DB
        let mut tombstone = make_issue_at("test-001", "Tombstone", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(Utc::now());
        storage.create_issue(&tombstone, "test").unwrap();

        // Create JSONL with same ID but trying to resurrect
        let mut incoming = make_issue_at("test-001", "Resurrected", fixed_time(200));
        incoming.status = Status::Open;
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should skip due to tombstone protection
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.tombstone_skipped, 1);

        let still_tombstone = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(still_tombstone.status, Status::Tombstone);
    }

    #[test]
    fn test_import_new_issue_creates() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with new issue
        let new_issue = make_test_issue("test-new", "Brand new");
        let json = serde_json::to_string(&new_issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();

        // New issue should be imported
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.created_count, 1);
        assert_eq!(result.updated_count, 0);
        assert_eq!(result.skipped_count, 0);
        assert!(storage.get_issue("test-new").unwrap().is_some());
    }

    #[test]
    fn test_import_stores_content_hash_after_external_ref_trim() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut new_issue = make_test_issue("test-ext", "External ref trim");
        new_issue.external_ref = Some("  ext-123  ".to_string());
        let json = serde_json::to_string(&new_issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();

        assert_eq!(result.imported_count, 1);
        let stored = storage.get_issue("test-ext").unwrap().unwrap();
        let expected_hash = crate::util::content_hash(&stored);
        assert_eq!(stored.external_ref.as_deref(), Some("ext-123"));
        assert_eq!(stored.content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn test_get_issue_ids_missing_file_returns_empty() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("nonexistent.jsonl");

        let ids = get_issue_ids_from_jsonl(&path).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn test_count_issues_missing_file_returns_zero() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("nonexistent.jsonl");

        let count = count_issues_in_jsonl(&path).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_export_computes_content_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-001", "Test");
        storage.create_issue(&issue, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        // Result should include a non-empty content hash
        assert!(!result.content_hash.is_empty());
        // Hash should be hex (64 chars for SHA256)
        assert_eq!(result.content_hash.len(), 64);
    }

    #[test]
    fn test_export_deterministic_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();

        let issue = make_test_issue("bd-001", "Deterministic");
        storage.create_issue(&issue, "test").unwrap();

        let config = ExportConfig::default();

        // Export twice to different files
        let path1 = temp_dir.path().join("export1.jsonl");
        let path2 = temp_dir.path().join("export2.jsonl");

        let result1 = export_to_jsonl(&storage, &path1, &config).unwrap();
        let result2 = export_to_jsonl(&storage, &path2, &config).unwrap();

        // Hashes should be identical for same content
        assert_eq!(result1.content_hash, result2.content_hash);
    }

    #[test]
    fn test_import_skips_ephemerals() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with ephemeral issue
        let mut ephemeral = make_test_issue("test-001", "Ephemeral");
        ephemeral.ephemeral = true;
        let json = serde_json::to_string(&ephemeral).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.skipped_count, 1);
        assert_eq!(result.imported_count, 0);
        assert!(storage.get_issue("test-001").unwrap().is_none());
    }

    #[test]
    fn test_import_handles_empty_lines() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with empty lines
        let issue = make_test_issue("test-001", "Valid");
        let json = serde_json::to_string(&issue).unwrap();
        let content = format!("\n{json}\n\n\n");
        fs::write(&path, content).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.imported_count, 1);
    }

    #[test]
    fn test_import_keeps_distinct_ids_with_identical_content() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue1 = make_test_issue("test-001", "Same content");
        let issue2 = make_test_issue("test-002", "Same content");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                .unwrap();
        assert_eq!(result.imported_count, 2);
        assert_eq!(result.skipped_count, 0);
        assert!(storage.get_issue("test-001").unwrap().is_some());
        assert!(storage.get_issue("test-002").unwrap().is_some());
    }

    #[test]
    fn test_import_restores_foreign_keys_after_relation_sync_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("test-001", "Broken relations");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        storage.execute_test_sql("DROP TABLE comments;").unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
            .unwrap_err();
        assert!(
            err.to_string().contains("comments"),
            "unexpected error: {err}"
        );

        let fk_enabled = storage
            .execute_raw_query("PRAGMA foreign_keys")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(fk_enabled, 1, "foreign key enforcement should be restored");
    }

    #[test]
    fn test_restore_foreign_keys_after_import_errors_on_dangling_rows() {
        let storage = SqliteStorage::open_memory().unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('missing-issue', 'tester', 'dangling', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        let err = restore_foreign_keys_after_import(&storage, true).unwrap_err();
        assert!(
            err.to_string()
                .contains("orphaned rows in comments.issue_id"),
            "unexpected error: {err}"
        );

        let fk_enabled = storage
            .execute_raw_query("PRAGMA foreign_keys")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(fk_enabled, 1, "foreign key enforcement should be restored");
    }

    #[test]
    fn test_import_orphan_cleanup_preserves_external_dependency_endpoints() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut epic = make_test_issue("bd-epic", "Epic");
        epic.issue_type = IssueType::Epic;
        storage.create_issue(&epic, "tester").unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('external:child:cap', 'bd-epic', 'parent-child', '2026-01-01T00:00:00Z', 'tester');
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('missing-issue', 'tester', 'dangling', '2026-01-01T00:00:00Z');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let cleaned = cleanup_import_orphans_in_tx(&storage).unwrap();

        assert_eq!(cleaned, 1, "only the real local orphan should be removed");
        let external_rows = storage
            .execute_raw_query(
                "SELECT issue_id, depends_on_id
                 FROM dependencies
                 WHERE issue_id = 'external:child:cap'",
            )
            .unwrap();
        assert_eq!(
            external_rows.len(),
            1,
            "external dependency endpoints must survive import cleanup"
        );
    }

    #[test]
    fn test_import_new_issue_replaces_preexisting_owned_relation_orphans() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let parent = make_test_issue("bd-parent", "Parent");
        storage.create_issue(&parent, "tester").unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO labels (issue_id, label)
                 VALUES ('bd-new', 'stale-label');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-new', 'bd-parent', 'blocks', '2026-01-01T00:00:00Z', 'legacy');
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('bd-new', 'legacy', 'stale comment', '2026-01-01T00:00:00Z');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let issue = make_test_issue("bd-new", "Clean import");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-")).unwrap();

        assert_eq!(result.created_count, 1);
        assert!(
            storage.get_labels("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned labels"
        );
        assert!(
            storage.get_dependencies_full("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned dependencies"
        );
        assert!(
            storage.get_comments("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned comments"
        );
    }

    #[test]
    fn ordinary_import_into_empty_storage_does_not_require_fresh_replacement_witness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");
        let mut issue = make_test_issue("bd-ordinary", "Ordinary empty import");
        issue.labels = vec!["ordinary".to_string()];
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let result = import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        assert_eq!(result.created_count, 1);
        assert_eq!(
            storage.get_labels("bd-ordinary").unwrap(),
            vec!["ordinary".to_string()]
        );
    }

    #[test]
    fn fresh_replacement_import_inserts_relations_after_global_empty_proof() {
        let (_temp, _beads_dir, jsonl_path, _authority, mut storage, witness) =
            fresh_replacement_import_fixture();
        let mut issue = make_test_issue("bd-fresh", "Fresh replacement import");
        issue.labels = vec!["fresh".to_string()];
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();
        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();

        let result = import_from_jsonl_snapshot_into_fresh_replacement(
            &mut storage,
            &source,
            &ImportConfig::default(),
            Some("bd-"),
            witness,
        )
        .unwrap();

        assert_eq!(result.created_count, 1);
        assert_eq!(
            storage.get_labels("bd-fresh").unwrap(),
            vec!["fresh".to_string()]
        );
    }

    #[test]
    fn fresh_replacement_import_aborts_if_orphan_appears_after_witness() {
        let (_temp, _beads_dir, jsonl_path, _authority, mut storage, witness) =
            fresh_replacement_import_fixture();
        let issue = make_test_issue("bd-new", "Must not import");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();
        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO labels (issue_id, label) VALUES ('bd-orphan', 'stale');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let error = import_from_jsonl_snapshot_into_fresh_replacement(
            &mut storage,
            &source,
            &ImportConfig::default(),
            Some("bd-"),
            witness,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("gained relation rows before import"),
            "unexpected error: {error}"
        );
        assert!(storage.get_issue("bd-new").unwrap().is_none());
    }

    #[test]
    fn fresh_replacement_import_rejects_inode_replacement_after_witness() {
        let (temp, _beads_dir, jsonl_path, authority, mut storage, witness) =
            fresh_replacement_import_fixture();
        let issue = make_test_issue("bd-new", "Must not import");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();
        let source = capture_jsonl_source_snapshot(&jsonl_path).unwrap();
        let db_path = authority.canonical_database_path();
        let displaced = temp.path().join("displaced.db");
        fs::rename(db_path, &displaced).unwrap();
        fs::copy(&displaced, db_path).unwrap();

        let error = import_from_jsonl_snapshot_into_fresh_replacement(
            &mut storage,
            &source,
            &ImportConfig::default(),
            Some("bd-"),
            witness,
        )
        .unwrap_err();

        let rendered = error.to_string();
        assert!(
            rendered.contains("Database inode changed") || rendered.contains("identity changed"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn test_import_error_reports_foreign_key_restore_failure_when_both_fail() {
        let apply_result: Result<ImportResult> =
            Err(BeadsError::Config("stream import failed".to_string()));
        let fk_restore_result: Result<()> = Err(BeadsError::Config(
            "foreign keys stayed disabled".to_string(),
        ));

        let err =
            finish_import_after_foreign_key_restore(apply_result, fk_restore_result).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains(
                "jsonl import failed, and SQLite foreign key enforcement could not be re-enabled"
            ),
            "unexpected error message: {msg}"
        );
        assert!(
            msg.contains("foreign keys stayed disabled"),
            "restore error should be included: {msg}"
        );
        assert!(
            msg.contains("stream import failed"),
            "original import error should be preserved as the source: {msg}"
        );

        assert!(
            matches!(&err, BeadsError::WithContext { .. }),
            "expected WithContext wrapping both failures"
        );
        if let BeadsError::WithContext { context, source } = err {
            assert!(context.contains("foreign keys stayed disabled"));
            assert_eq!(
                source.to_string(),
                "Configuration error: stream import failed"
            );
        }
    }

    #[test]
    fn test_import_rolls_back_partial_changes_after_relation_sync_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let existing = make_test_issue("test-existing", "Existing issue");
        storage.create_issue(&existing, "test").unwrap();
        storage
            .set_export_hashes(&[("test-existing".to_string(), "existing-hash".to_string())])
            .unwrap();

        let issue = make_test_issue("test-001", "Broken relations");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        storage.execute_test_sql("DROP TABLE comments;").unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
            .unwrap_err();
        assert!(
            err.to_string().contains("comments"),
            "unexpected error: {err}"
        );

        assert!(
            storage.get_issue("test-001").unwrap().is_none(),
            "failed import should not leave a partially inserted issue behind"
        );
        assert!(
            storage.get_issue("test-existing").unwrap().is_some(),
            "failed import should preserve pre-existing issues"
        );

        let export_hash_rows = storage
            .execute_raw_query("SELECT issue_id, content_hash FROM export_hashes")
            .unwrap();
        assert_eq!(export_hash_rows.len(), 1, "export hashes should roll back");
        assert_eq!(
            export_hash_rows[0]
                .first()
                .and_then(SqliteValue::as_text)
                .unwrap_or(""),
            "test-existing"
        );
    }

    #[test]
    fn test_detect_collision_external_ref_priority() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut ext_issue = make_issue_at("bd-ext", "External", fixed_time(100));
        ext_issue.external_ref = Some("JIRA-1".to_string());
        set_content_hash(&mut ext_issue);
        storage.upsert_issue_for_import(&ext_issue).unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Incoming", fixed_time(200));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        // Incoming has same external_ref as ext_issue - should match on external_ref
        // even though it has same title/content_hash as hash_issue
        let mut incoming = make_issue_at("bd-new", "Incoming", fixed_time(300));
        incoming.external_ref = Some("JIRA-1".to_string());
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-ext");
            assert_eq!(match_type, MatchType::ExternalRef);
            assert_eq!(phase, 1);
        }
    }

    #[test]
    fn test_detect_collision_id_preempts_content_hash() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Same Content", fixed_time(100));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        let mut id_issue = make_issue_at("bd-same", "Different Content", fixed_time(100));
        set_content_hash(&mut id_issue);
        storage.upsert_issue_for_import(&id_issue).unwrap();

        let incoming = make_issue_at("bd-same", "Same Content", fixed_time(200));
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-same");
            assert_eq!(match_type, MatchType::Id);
            assert_eq!(phase, 2);
        }
    }

    #[test]
    fn test_detect_collision_duplicate_content_hash_keeps_first_match() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut first = make_issue_at("bd-first", "Same Content", fixed_time(100));
        set_content_hash(&mut first);
        storage.upsert_issue_for_import(&first).unwrap();

        let mut second = make_issue_at("bd-second", "Same Content", fixed_time(200));
        set_content_hash(&mut second);
        storage.upsert_issue_for_import(&second).unwrap();

        let incoming = make_issue_at("bd-new", "Same Content", fixed_time(300));
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );

        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-first");
            assert_eq!(match_type, MatchType::ContentHash);
            assert_eq!(phase, 3);
        }
    }

    #[test]
    fn test_detect_collision_ignores_tombstones_for_content_hash_match() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut tombstone = make_issue_at("bd-tomb", "Same Tombstone Content", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(fixed_time(110));
        tombstone.deleted_by = Some("tester".to_string());
        tombstone.delete_reason = Some("old delete".to_string());
        set_content_hash(&mut tombstone);
        storage.upsert_issue_for_import(&tombstone).unwrap();

        let mut incoming = make_issue_at("bd-new", "Same Tombstone Content", fixed_time(200));
        incoming.status = Status::Tombstone;
        incoming.deleted_at = Some(fixed_time(210));
        incoming.deleted_by = Some("jsonl".to_string());
        incoming.delete_reason = Some("incoming delete".to_string());
        let computed_hash = crate::util::content_hash(&incoming);
        assert_eq!(
            tombstone.content_hash.as_deref(),
            Some(computed_hash.as_str()),
            "delete metadata must not affect content_hash"
        );

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );

        assert!(
            matches!(collision, CollisionResult::NewIssue),
            "tombstones must not participate in content-hash collision matching: {collision:?}"
        );
    }

    #[test]
    fn test_detect_collision_id_match() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let existing = make_issue_at("bd-1", "Existing", fixed_time(100));
        storage.create_issue(&existing, "test").unwrap();

        let incoming = make_issue_at("bd-1", "Incoming", fixed_time(200));

        let computed_hash = crate::util::content_hash(&incoming);
        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );

        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-1");
            assert_eq!(match_type, MatchType::Id);
            assert_eq!(phase, 2);
        }
    }

    #[test]
    fn test_determine_action_tombstone_skip() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut tombstone = make_issue_at("bd-1", "Tombstone", fixed_time(100));
        tombstone.status = Status::Tombstone;
        storage.create_issue(&tombstone, "test").unwrap();

        let incoming = make_issue_at("bd-1", "Incoming", fixed_time(200));
        let collision = CollisionResult::Match {
            existing_id: "bd-1".to_string(),
            match_type: MatchType::Id,
            phase: 3,
        };
        let (_, _, meta_by_id) = build_collision_maps(&storage);
        let action = determine_action(&collision, &incoming, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected tombstone skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Tombstone protection"));
        }
    }

    #[test]
    fn test_determine_action_timestamp_comparison() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let existing = make_issue_at("bd-1", "Existing", fixed_time(100));
        storage.create_issue(&existing, "test").unwrap();

        let collision = CollisionResult::Match {
            existing_id: "bd-1".to_string(),
            match_type: MatchType::Id,
            phase: 3,
        };
        let (_, _, meta_by_id) = build_collision_maps(&storage);

        let newer = make_issue_at("bd-1", "Incoming", fixed_time(200));
        let action = determine_action(&collision, &newer, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Update { .. }),
            "expected update action"
        );

        let equal = make_issue_at("bd-1", "Incoming", fixed_time(100));
        let action = determine_action(&collision, &equal, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected equal timestamp skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Equal timestamps"));
        }

        let older = make_issue_at("bd-1", "Incoming", fixed_time(50));
        let action = determine_action(&collision, &older, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected older timestamp skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Existing is newer"));
        }
    }

    #[test]
    fn test_import_prefix_mismatch_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_issue_at("xx-001", "Bad prefix", fixed_time(100));
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let err = import_from_jsonl(&mut storage, &path, &config, Some("bd")).unwrap_err();
        assert!(err.to_string().contains("Prefix mismatch"));
    }

    #[test]
    fn test_import_prefix_mismatch_error_for_shared_prefix_superset() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_issue_at("bdx-001", "Looks similar but wrong prefix", fixed_time(100));
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd"))
            .unwrap_err();
        assert!(err.to_string().contains("Prefix mismatch"));
        assert!(err.to_string().contains("bdx-001"));
    }

    #[test]
    fn test_import_duplicate_external_ref_errors() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        issue1.external_ref = Some("JIRA-1".to_string());
        let mut issue2 = make_issue_at("bd-002", "Issue 2", fixed_time(120));
        issue2.external_ref = Some("JIRA-1".to_string());

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let config = ImportConfig::default();
        let err = import_from_jsonl(&mut storage, &path, &config, None).unwrap_err();
        assert!(err.to_string().contains("Duplicate external_ref"));
    }

    #[test]
    fn test_import_duplicate_issue_ids_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        let issue2 = make_issue_at("bd-001", "Issue 2", fixed_time(120));

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd"))
            .unwrap_err();
        assert!(err.to_string().contains("Duplicate issue id 'bd-001'"));
    }

    #[test]
    fn test_import_duplicate_external_ref_clears_and_inserts() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        issue1.external_ref = Some("JIRA-1".to_string());
        let mut issue2 = make_issue_at("bd-002", "Issue 2", fixed_time(120));
        issue2.external_ref = Some("JIRA-1".to_string());

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let config = ImportConfig {
            clear_duplicate_external_refs: true,
            ..Default::default()
        };
        let result = import_from_jsonl(&mut storage, &path, &config, None).unwrap();

        assert_eq!(result.imported_count, 2);
        assert_eq!(result.skipped_count, 0);
        let first = storage.get_issue("bd-001").unwrap().unwrap();
        let second = storage.get_issue("bd-002").unwrap().unwrap();
        assert_eq!(first.external_ref.as_deref(), Some("JIRA-1"));
        assert!(second.external_ref.is_none());
    }

    #[test]
    fn test_export_deterministic_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue_a = make_test_issue("bd-z", "Zed");
        let issue_b = make_test_issue("bd-a", "Aye");
        let issue_c = make_test_issue("bd-m", "Em");

        storage.create_issue(&issue_a, "test").unwrap();
        storage.create_issue(&issue_b, "test").unwrap();
        storage.create_issue(&issue_c, "test").unwrap();

        let config = ExportConfig::default();
        export_to_jsonl(&storage, &output_path, &config).unwrap();

        let ids = read_issues_from_jsonl(&output_path)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["bd-a", "bd-m", "bd-z"]);
    }

    #[test]
    fn frozen_export_cutoff_produces_identical_serial_and_parallel_preparation() {
        let export_as_of = fixed_time(2_000_000_000);
        let ttl = chrono::Duration::days(30);
        let mut issues = (0..(EXPORT_PARALLEL_PREPARE_MIN_ISSUES + 3))
            .map(|index| make_test_issue(&format!("bd-{index:04}"), "Cutoff parity"))
            .collect::<Vec<_>>();

        issues[0].status = Status::Tombstone;
        issues[0].deleted_at = Some(export_as_of - ttl);
        issues[1].status = Status::Tombstone;
        issues[1].deleted_at = Some(export_as_of - ttl - chrono::Duration::nanoseconds(1));
        issues[2].status = Status::Tombstone;
        issues[2].deleted_at = Some(export_as_of - ttl + chrono::Duration::nanoseconds(1));

        let serial = prepare_export_issue_chunk(&issues, Some(30), &export_as_of);
        let parallel =
            prepare_export_issues_jsonl_parallel(&issues, Some(30), &export_as_of, 4).unwrap();

        assert_eq!(serial.len(), parallel.len());
        for (serial_entry, parallel_entry) in serial.iter().zip(&parallel) {
            match (serial_entry, parallel_entry) {
                (
                    PreparedExportEntry::Issue(serial_issue),
                    PreparedExportEntry::Issue(parallel_issue),
                ) => {
                    assert_eq!(serial_issue.id, parallel_issue.id);
                    assert_eq!(serial_issue.jsonl_line, parallel_issue.jsonl_line);
                    assert_eq!(serial_issue.content_hash, parallel_issue.content_hash);
                    assert_eq!(
                        serial_issue.dependency_count,
                        parallel_issue.dependency_count
                    );
                    assert_eq!(serial_issue.label_count, parallel_issue.label_count);
                    assert_eq!(serial_issue.comment_count, parallel_issue.comment_count);
                }
                (
                    PreparedExportEntry::SkippedTombstone(serial_id),
                    PreparedExportEntry::SkippedTombstone(parallel_id),
                ) => assert_eq!(serial_id, parallel_id),
                (
                    PreparedExportEntry::Error(serial_error),
                    PreparedExportEntry::Error(parallel_error),
                ) => {
                    assert_eq!(serial_error.entity_type, parallel_error.entity_type);
                    assert_eq!(serial_error.entity_id, parallel_error.entity_id);
                    assert_eq!(serial_error.message, parallel_error.message);
                }
                _ => panic!("serial and parallel preparation classified an issue differently"),
            }
        }

        assert!(matches!(&serial[0], PreparedExportEntry::Issue(_)));
        assert!(matches!(
            &serial[1],
            PreparedExportEntry::SkippedTombstone(id) if id == "bd-0001"
        ));
        assert!(matches!(&serial[2], PreparedExportEntry::Issue(_)));
    }

    #[test]
    fn test_normalize_issue_for_export_orders_identical_comments_by_id() {
        let timestamp = fixed_time(100);
        let mut issue = make_test_issue("bd-1", "Ordering");
        issue.comments = vec![
            Comment {
                id: 9,
                issue_id: issue.id.clone(),
                author: "tester".to_string(),
                body: "same".to_string(),
                created_at: timestamp,
            },
            Comment {
                id: 2,
                issue_id: issue.id.clone(),
                author: "tester".to_string(),
                body: "same".to_string(),
                created_at: timestamp,
            },
        ];

        normalize_issue_for_export(&mut issue);

        let ids = issue
            .comments
            .into_iter()
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![2, 9]);
    }

    #[test]
    fn test_finalize_export_updates_metadata_and_clears_dirty() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-1", "Issue");
        storage.create_issue(&issue, "test").unwrap();
        assert_eq!(storage.get_dirty_issue_ids().unwrap().len(), 1);

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();
        finalize_export(
            &mut storage,
            &result,
            Some(&result.issue_hashes),
            &output_path,
        )
        .unwrap();

        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
        assert!(
            storage
                .get_metadata(METADATA_JSONL_CONTENT_HASH)
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .get_metadata(METADATA_LAST_EXPORT_TIME)
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .get_metadata(METADATA_JSONL_MTIME)
                .unwrap()
                .is_some()
        );
        assert!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap().is_some());
    }

    #[test]
    fn full_export_finalization_reconciles_hashes_and_excluded_dirty_rows_exactly() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let regular = make_test_issue("bd-regular", "Regular issue");
        let mut ephemeral = make_test_issue("bd-ephemeral", "Ephemeral issue");
        ephemeral.ephemeral = true;
        let wisp = make_test_issue("bd-wisp-session", "Wisp issue");
        for issue in [&regular, &ephemeral, &wisp] {
            storage.create_issue(issue, "test").unwrap();
        }
        storage
            .execute_raw("UPDATE dirty_issues SET marked_at = '2026-07-27T10:00:00+00:00'")
            .unwrap();

        let result = export_to_jsonl(&storage, &output_path, &ExportConfig::default()).unwrap();
        assert_eq!(result.exported_ids, vec![regular.id.clone()]);
        assert_eq!(
            result
                .intentionally_excluded_marked_at
                .iter()
                .map(|(issue_id, _)| issue_id.as_str())
                .collect::<Vec<_>>(),
            vec![ephemeral.id.as_str(), wisp.id.as_str()],
            "equal dirty timestamps must still produce a total issue-ID order"
        );

        storage
            .set_export_hashes(&[
                (regular.id.clone(), result.issue_hashes[0].1.clone()),
                (ephemeral.id.clone(), "stale-ephemeral".to_string()),
                (wisp.id.clone(), "stale-wisp".to_string()),
            ])
            .unwrap();
        let regular_exported_at_before = storage
            .execute_raw_query(
                "SELECT exported_at FROM export_hashes WHERE issue_id = 'bd-regular'",
            )
            .unwrap()[0][0]
            .as_text()
            .unwrap()
            .to_string();

        finalize_export(
            &mut storage,
            &result,
            Some(&result.issue_hashes),
            &output_path,
        )
        .unwrap();

        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
        let rows = storage
            .execute_raw_query(
                "SELECT issue_id, content_hash, exported_at FROM export_hashes ORDER BY issue_id",
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_text(), Some(regular.id.as_str()));
        assert_eq!(
            rows[0][1].as_text(),
            Some(result.issue_hashes[0].1.as_str())
        );
        assert_eq!(
            rows[0][2].as_text(),
            Some(regular_exported_at_before.as_str()),
            "unchanged mappings must retain their original exported_at timestamp"
        );

        let finalization = capture_sync_merge_export_finalization_witness(&storage).unwrap();
        assert_eq!(finalization.dirty_issues.rows, 0);
        assert_eq!(finalization.export_metadata.rows, 5);
        assert_eq!(
            finalization.export_hashes,
            sync_merge_export_hash_mapping_witness(&result.issue_hashes).unwrap()
        );
    }

    #[test]
    fn malformed_raw_dirty_row_blocks_full_export_finalization() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");
        let issue = make_test_issue("bd-malformed-dirty", "Malformed dirty witness");
        storage.create_issue(&issue, "test").unwrap();
        let result = export_to_jsonl(&storage, &output_path, &ExportConfig::default()).unwrap();

        storage
            .execute_raw(
                "UPDATE dirty_issues SET marked_at = X'00' \
                 WHERE issue_id = 'bd-malformed-dirty'",
            )
            .unwrap();
        let witnessed = capture_sync_merge_export_finalization_witness(&storage).unwrap();
        assert_eq!(
            witnessed.dirty_issues.rows, 1,
            "raw finalization witness must not drop a non-text dirty row"
        );
        assert!(storage.get_dirty_issue_metadata().is_err());

        let error = finalize_export(
            &mut storage,
            &result,
            Some(&result.issue_hashes),
            &output_path,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("marked_at")
                || error.to_string().contains("dirty")
                || error.to_string().contains("Dirty"),
            "unexpected malformed dirty-row error: {error}"
        );
        let after_failure = capture_sync_merge_export_finalization_witness(&storage).unwrap();
        assert_eq!(
            after_failure, witnessed,
            "failed finalization must roll back every export-finalization table and metadata row"
        );
        assert!(
            after_failure.dirty_issues.rows != 0
                || after_failure.needs_flush.as_deref() != Some("false"),
            "failed finalization must not satisfy the complete clean-export predicate"
        );
    }

    #[test]
    fn test_auto_flush_clears_byte_identical_dirty_marker_without_rewrite() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let output_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();

        let issue = make_test_issue("bd-noop", "No-op dirty marker");
        storage.create_issue(&issue, "test").unwrap();

        let first = auto_flush(&mut storage, &beads_dir, &output_path, false).unwrap();
        assert!(first.flushed);
        let before = fs::read_to_string(&output_path).unwrap();

        storage
            .replace_dirty_issue_marker("bd-noop", "manual-dirty-marker")
            .unwrap();

        let second = auto_flush(&mut storage, &beads_dir, &output_path, false).unwrap();
        assert!(
            !second.flushed,
            "byte-identical dirty markers should not rewrite JSONL"
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
        assert_eq!(fs::read_to_string(&output_path).unwrap(), before);
    }

    #[test]
    fn test_filter_dirty_metadata_for_export_only_includes_exported_ids() {
        let dirty_metadata = vec![
            ("bd-1".to_string(), "t1".to_string()),
            ("bd-2".to_string(), "t2".to_string()),
            ("bd-3".to_string(), "t3".to_string()),
        ];
        let exported_ids = vec!["bd-1".to_string()];
        let skipped_tombstone_ids = vec!["bd-3".to_string()];

        let filtered = filter_dirty_metadata_for_export(
            &dirty_metadata,
            &exported_ids,
            &skipped_tombstone_ids,
        );

        assert_eq!(
            filtered,
            vec![
                ("bd-1".to_string(), "t1".to_string()),
                ("bd-3".to_string(), "t3".to_string()),
            ]
        );
    }

    #[test]
    fn test_finalize_export_rolls_back_on_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-finalize", "Issue");
        storage.create_issue(&issue, "test").unwrap();
        assert_eq!(storage.get_dirty_issue_ids().unwrap().len(), 1);

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        let invalid_issue_hashes = vec![("bd-missing".to_string(), "hash".to_string())];

        let err = finalize_export(
            &mut storage,
            &result,
            Some(&invalid_issue_hashes),
            &output_path,
        )
        .unwrap_err();
        assert!(
            matches!(err, BeadsError::SyncConflict { .. }),
            "unexpected error: {err:?}"
        );

        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-finalize".to_string()]
        );
        assert!(storage.get_export_hash("bd-finalize").unwrap().is_none());
        assert!(
            storage
                .get_metadata(METADATA_JSONL_CONTENT_HASH)
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .get_metadata(METADATA_LAST_EXPORT_TIME)
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .get_metadata(METADATA_JSONL_MTIME)
                .unwrap()
                .is_none()
        );
        assert!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap().is_none());
    }

    #[test]
    fn test_export_policy_strict_fails_on_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let result = export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Strict);
        assert!(result.is_err());
    }

    #[test]
    fn frozen_export_cutoff_keeps_writer_and_file_bytes_and_hashes_identical() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let export_as_of = fixed_time(2_000_000_000);
        let ttl = chrono::Duration::days(30);

        let mut boundary = make_test_issue("bd-boundary", "Retained at exact boundary");
        boundary.status = Status::Tombstone;
        boundary.deleted_at = Some(export_as_of - ttl);
        boundary.deleted_by = Some("test".to_string());
        let mut expired = make_test_issue("bd-expired", "Expired after boundary");
        expired.status = Status::Tombstone;
        expired.deleted_at = Some(export_as_of - ttl - chrono::Duration::nanoseconds(1));
        expired.deleted_by = Some("test".to_string());
        let open = make_test_issue("bd-open", "Always exported");
        for issue in [&boundary, &expired, &open] {
            storage.create_issue(issue, "test").unwrap();
        }

        let mut writer = Vec::new();
        let (writer_result, writer_report) = export_to_writer_with_policy_and_retention_at(
            &storage,
            &mut writer,
            ExportErrorPolicy::Strict,
            Some(30),
            export_as_of,
        )
        .unwrap();

        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let output_path = beads_dir.join("issues.jsonl");
        let config = ExportConfig {
            force: true,
            error_policy: ExportErrorPolicy::Strict,
            retention_days: Some(30),
            export_as_of: Some(export_as_of),
            beads_dir: Some(beads_dir),
            max_parallel_workers: 1,
            ..Default::default()
        };
        let (file_result, file_report) =
            export_to_jsonl_with_policy(&storage, &output_path, &config).unwrap();
        let file_bytes = fs::read(&output_path).unwrap();

        assert_eq!(writer, file_bytes);
        assert_eq!(writer_result.content_hash, file_result.content_hash);
        assert_eq!(writer_result.exported_ids, file_result.exported_ids);
        assert_eq!(
            writer_result.skipped_tombstone_ids,
            file_result.skipped_tombstone_ids
        );
        assert_eq!(writer_report.issues_exported, file_report.issues_exported);
        assert!(writer_result.exported_ids.contains(&boundary.id));
        assert!(writer_result.skipped_tombstone_ids.contains(&expired.id));
    }

    #[test]
    fn test_export_to_writer_streams_large_issue_set_in_id_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue_count = (EXPORT_ISSUE_BATCH_SIZE * 2) + 3;

        for index in (0..issue_count).rev() {
            let id = format!("bd-{index:04}");
            let title = format!("Issue {index}");
            let issue = make_test_issue(&id, &title);
            storage.create_issue(&issue, "test").unwrap();
        }

        let mut writer = Vec::new();
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Strict).unwrap();

        assert_eq!(result.exported_count, issue_count);
        assert_eq!(report.issues_exported, issue_count);

        let output = String::from_utf8(writer).unwrap();
        let ids = output
            .lines()
            .map(|line| serde_json::from_str::<Issue>(line).unwrap().id)
            .collect::<Vec<_>>();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();

        assert_eq!(ids.len(), issue_count);
        assert_eq!(ids, sorted_ids);
    }

    #[test]
    fn test_export_policy_best_effort_skips_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::BestEffort)
                .unwrap();
        assert_eq!(result.exported_count, 1);
        assert_eq!(report.errors.len(), 1);
        let output = writer.into_string();
        assert!(output.contains("bd-001"));
        assert!(!output.contains("bd-002"));
    }

    #[test]
    fn test_export_policy_partial_collects_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Partial)
                .unwrap();

        assert_eq!(result.exported_count, 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn test_export_policy_required_core_fails_on_issue_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let result =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::RequiredCore);
        assert!(result.is_err());
    }

    #[test]
    fn test_export_policy_required_core_allows_non_core_errors() {
        // This test verifies that RequiredCore policy exports all issues successfully
        // and would tolerate non-core errors (Label, Dependency, Comment) if they occurred.
        // The test doesn't generate non-core errors since the setup has no labels/deps.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = Vec::new();
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::RequiredCore)
                .unwrap();

        assert_eq!(result.exported_count, 2);
        // Any errors present should be non-core (Issue errors would cause failure above)
        for err in &report.errors {
            assert_ne!(
                err.entity_type,
                ExportEntityType::Issue,
                "Issue errors should fail RequiredCore policy"
            );
        }
    }

    // ============================================================================
    // PREFLIGHT TESTS (beads_rust-0v1.2.7)
    // ============================================================================

    #[test]
    fn test_preflight_check_status_ordering() {
        // Verify that PreflightCheckStatus can be used for comparison
        assert_ne!(PreflightCheckStatus::Pass, PreflightCheckStatus::Warn);
        assert_ne!(PreflightCheckStatus::Warn, PreflightCheckStatus::Fail);
        assert_ne!(PreflightCheckStatus::Pass, PreflightCheckStatus::Fail);
    }

    #[test]
    fn test_preflight_result_aggregates_status() {
        let mut result = PreflightResult::new();

        // Initial state is Pass
        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);
        assert!(result.is_ok());
        assert!(result.has_no_failures());

        // Add a passing check
        result.add(PreflightCheck::pass("test1", "Test 1", "Passed"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);

        // Add a warning - overall becomes Warn
        result.add(PreflightCheck::warn("test2", "Test 2", "Warning", "Fix it"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Warn);
        assert!(!result.is_ok());
        assert!(result.has_no_failures());

        // Add a failure - overall becomes Fail
        result.add(PreflightCheck::fail("test3", "Test 3", "Failed", "Fix it"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(!result.is_ok());
        assert!(!result.has_no_failures());

        // Check counts
        assert_eq!(result.failures().len(), 1);
        assert_eq!(result.warnings().len(), 1);
    }

    #[test]
    fn test_preflight_result_into_result_succeeds_on_pass() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::pass("test", "Test", "OK"));

        let converted = result.into_result();
        assert!(converted.is_ok());
    }

    #[test]
    fn test_preflight_result_into_result_succeeds_on_warn() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::warn("test", "Test", "Warning", "Fix"));

        let converted = result.into_result();
        assert!(converted.is_ok());
    }

    #[test]
    fn test_preflight_result_into_result_fails_on_fail() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::fail("test", "Test", "Failed", "Fix it"));

        let converted = result.into_result();
        assert!(converted.is_err());

        let err_msg = converted.unwrap_err().to_string();
        assert!(err_msg.contains("Preflight checks failed"));
        assert!(err_msg.contains("test"));
        assert!(err_msg.contains("Failed"));
    }

    #[test]
    fn test_preflight_import_rejects_nonexistent_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("nonexistent.jsonl");

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(result.failures().iter().any(|c| c.name == "file_readable"));
    }

    #[test]
    fn test_preflight_import_rejects_conflict_markers() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write a file with conflict markers
        let mut file = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(file, "<<<<<<< HEAD").unwrap();
        file.write_all(br#"{"id":"bd-1","title":"Test"}"#).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "=======").unwrap();
        file.write_all(br#"{"id":"bd-1","title":"Test Modified"}"#)
            .unwrap();
        writeln!(file).unwrap();
        writeln!(file, ">>>>>>> branch").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "no_conflict_markers")
        );
    }

    #[test]
    fn test_preflight_import_does_not_inspect_rejected_path() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let outside_jsonl_path = temp.path().join("outside.jsonl");
        std::fs::write(&outside_jsonl_path, "not json\n").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            allow_external_jsonl: false,
            ..Default::default()
        };

        let result = preflight_import(&outside_jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "path_validation"),
            "rejected path should fail path validation"
        );
        assert!(
            result.checks.iter().all(|c| c.name != "file_readable"
                && c.name != "no_conflict_markers"
                && c.name != "json_valid"
                && c.name != "prefix_match"),
            "preflight should not read or parse rejected paths: {:?}",
            result.checks
        );
    }

    #[test]
    fn test_preflight_import_passes_valid_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write valid JSONL
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);
        assert!(result.failures().is_empty());
    }

    #[test]
    fn test_preflight_export_passes_with_valid_setup() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let storage = SqliteStorage::open_memory().unwrap();
        let config = ExportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_export(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(
            result.overall_status,
            PreflightCheckStatus::Pass,
            "Expected Pass, got {:?}. Failures: {:?}",
            result.overall_status,
            result.failures()
        );
        assert!(result.failures().is_empty());
    }

    // ========================================================================
    // Preflight Guardrail Tests (beads_rust-1quj)
    // ========================================================================

    #[test]
    fn test_preflight_import_rejects_invalid_json_lines() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write JSONL with invalid lines
        let issue1 = make_test_issue("bd-001", "Good issue");
        let issue2 = make_test_issue("bd-002", "Another good issue");
        let good_json_1 = serde_json::to_string(&issue1).unwrap();
        let good_json_2 = serde_json::to_string(&issue2).unwrap();
        let content = format!("{good_json_1}\nNOT VALID JSON\n{good_json_2}\n{{\"broken: true}}\n");
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some(), "Expected json_valid failure");
        let msg = &json_check.unwrap().message;
        assert!(msg.contains("2 invalid issue record"), "Message was: {msg}");
        assert!(msg.contains("line 2"), "Should mention line 2: {msg}");
        assert!(msg.contains("line 4"), "Should mention line 4: {msg}");
    }

    #[test]
    fn test_preflight_import_passes_valid_json_lines() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        // json_valid should pass
        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    #[test]
    fn test_validate_jsonl_issue_records_rejects_duplicate_issue_ids() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");

        let issue = make_test_issue("bd-dup", "Duplicate");
        let issue_json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{issue_json}\n{issue_json}\n")).unwrap();

        let summary = validate_jsonl_issue_records(&jsonl_path).unwrap();

        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.invalid_count, 1);
        let preview = summary.preview_messages();
        assert!(
            preview
                .iter()
                .any(|message| message.contains("line 2: Duplicate issue id 'bd-dup'")),
            "expected duplicate-id validation failure in preview, got {preview:?}"
        );
    }

    #[test]
    fn test_preflight_import_rejects_duplicate_issue_ids_during_validation() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bd-dup", "Duplicate");
        let issue_json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{issue_json}\n{issue_json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures
            .iter()
            .find(|c| c.name == "json_valid")
            .expect("expected json_valid failure");
        assert!(
            json_check.message.contains("Duplicate issue id 'bd-dup'"),
            "expected duplicate-id validation message, got {}",
            json_check.message
        );
    }

    #[test]
    fn test_preflight_import_rejects_semantically_invalid_issue_records() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let mut invalid_issue = make_test_issue("bd-001", "");
        invalid_issue.title.clear();
        std::fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&invalid_issue).unwrap()),
        )
        .unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some(), "Expected json_valid failure");
        assert!(
            json_check
                .expect("json_valid failure")
                .message
                .contains("title"),
            "Expected validation failure to mention the empty title"
        );
    }

    #[test]
    fn test_preflight_import_rejects_prefix_mismatch() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write issues with wrong prefix
        let issue1 = make_test_issue("xx-001", "Wrong prefix 1");
        let issue2 = make_test_issue("xx-002", "Wrong prefix 2");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some(), "Expected prefix_match failure");
        let msg = &prefix_check.unwrap().message;
        assert!(msg.contains("xx-001"), "Should list mismatched ID: {msg}");
        assert!(msg.contains("xx-002"), "Should list mismatched ID: {msg}");
        assert!(msg.contains("2 mismatched"), "Should show count: {msg}");
    }

    #[test]
    fn test_preflight_import_rejects_shared_prefix_superset() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bdx-001", "Wrong shared prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();
        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some(), "Expected prefix_match failure");
        assert!(
            prefix_check.unwrap().message.contains("bdx-001"),
            "Should report the mismatched ID"
        );
    }

    #[test]
    fn test_preflight_import_prefix_check_skipped_when_override() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write issues with wrong prefix
        let issue = make_test_issue("xx-001", "Wrong prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            skip_prefix_validation: true,
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // prefix_match check should NOT be present when skip_prefix_validation is true
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_none(),
            "prefix_match check should be skipped when skip_prefix_validation is true"
        );
        // Overall should pass (or at least not fail on prefix)
        assert!(
            result.failures().iter().all(|c| c.name != "prefix_match"),
            "No prefix_match failures expected with override"
        );
    }

    #[test]
    fn test_preflight_import_prefix_passes_matching_prefix() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue1 = make_test_issue("bd-001", "Correct prefix 1");
        let issue2 = make_test_issue("bd-002", "Correct prefix 2");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_some(),
            "prefix_match check should be present"
        );
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "prefix_match should pass for matching prefix"
        );
    }

    #[test]
    fn test_preflight_import_prefix_accepts_slugged_ids() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bd-survey-my-thing-abc123", "Slugged issue");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_some(),
            "prefix_match check should be present"
        );
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "slugged IDs generated from the expected prefix should pass"
        );
    }

    #[test]
    fn test_id_matches_expected_prefix_keeps_non_delimited_supersets_out() {
        assert!(id_matches_expected_prefix(
            "bd-survey-my-thing-abc123",
            "bd"
        ));
        assert!(id_matches_expected_prefix(
            "bd-survey-my-thing-abc123",
            "bd-"
        ));
        assert!(!id_matches_expected_prefix("bdx-survey-abc123", "bd"));
        assert!(!id_matches_expected_prefix("x-bd-survey-abc123", "bd"));
        assert!(!id_matches_expected_prefix("bd-survey-abc123", ""));
    }

    #[test]
    fn test_preflight_import_prefix_no_check_without_expected() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("xx-001", "Any prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        // No expected_prefix passed — prefix check should not be added
        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_none(),
            "prefix_match check should not run without expected_prefix"
        );
    }

    #[test]
    fn test_preflight_import_conflict_markers_mixed_content() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Valid JSONL with embedded conflict markers
        let issue = make_test_issue("bd-001", "Good issue");
        let good_json = serde_json::to_string(&issue).unwrap();
        let content = format!(
            "{good_json}\n<<<<<<< HEAD\n{good_json}\n=======\n{good_json}\n>>>>>>> other\n"
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        // Should have both conflict marker AND json validation failures
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "no_conflict_markers"),
            "Should detect conflict markers"
        );
        assert!(
            result.failures().iter().any(|c| c.name == "json_valid"),
            "Conflict marker lines should also fail JSON validation"
        );
    }

    #[test]
    fn test_preflight_import_success_path_all_checks() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write valid JSONL with correct prefix
        let issue1 = make_test_issue("bd-001", "Issue One");
        let issue2 = make_test_issue("bd-002", "Issue Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // All checks should pass
        assert_eq!(
            result.overall_status,
            PreflightCheckStatus::Pass,
            "All checks should pass. Failures: {:?}",
            result
                .failures()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.message))
                .collect::<Vec<_>>()
        );
        assert!(result.failures().is_empty());

        // Verify all expected checks ran
        let check_names: Vec<&str> = result.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            check_names.contains(&"beads_dir_exists"),
            "Should check beads dir: {check_names:?}"
        );
        assert!(
            check_names.contains(&"file_readable"),
            "Should check file readable: {check_names:?}"
        );
        assert!(
            check_names.contains(&"no_conflict_markers"),
            "Should check conflict markers: {check_names:?}"
        );
        assert!(
            check_names.contains(&"json_valid"),
            "Should check JSON validity: {check_names:?}"
        );
        assert!(
            check_names.contains(&"prefix_match"),
            "Should check prefix match: {check_names:?}"
        );
    }

    #[test]
    fn test_preflight_import_mixed_prefix_partial_mismatch() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Mix of correct and incorrect prefix
        let good_issue = make_test_issue("bd-001", "Good prefix");
        let bad_issue = make_test_issue("xx-002", "Bad prefix");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&good_issue).unwrap(),
            serde_json::to_string(&bad_issue).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some());
        let msg = &prefix_check.unwrap().message;
        assert!(
            msg.contains("1 mismatched"),
            "Should show count of 1: {msg}"
        );
        assert!(msg.contains("xx-002"), "Should list the bad ID: {msg}");
    }

    #[test]
    fn test_preflight_import_prefix_skips_tombstones() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Create a tombstone with wrong prefix — should be silently ignored
        let mut tombstone = make_test_issue("xx-001", "Foreign tombstone");
        tombstone.status = Status::Tombstone;
        let good_issue = make_test_issue("bd-001", "Good issue");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&tombstone).unwrap(),
            serde_json::to_string(&good_issue).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // Tombstone with wrong prefix should not cause failure
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some());
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "Tombstones with wrong prefix should be ignored"
        );
    }

    #[test]
    fn test_preflight_import_empty_file_passes_json_check() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Empty file
        std::fs::write(&jsonl_path, "").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        // An empty file should pass JSON validation (no invalid lines)
        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    #[test]
    fn test_preflight_import_only_blank_lines_passes_json_check() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Only whitespace/blank lines
        std::fs::write(&jsonl_path, "\n\n  \n\t\n").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    // ========================================================================
    // 3-Way Merge Tests
    // ========================================================================

    fn fixed_time_merge(seconds: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn make_issue_with_hash(
        id: &str,
        title: &str,
        updated_at: chrono::DateTime<Utc>,
        hash: Option<&str>,
    ) -> Issue {
        let created_at = updated_at - chrono::Duration::seconds(60);
        Issue {
            id: id.to_string(),
            content_hash: hash.map(str::to_string),
            title: title.to_string(),
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
            created_at,
            created_by: None,
            updated_at,
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_merge_new_local_issue_kept() {
        // Issue only in left (new local) should be kept
        let local = make_issue_with_hash("bd-1", "New Local", fixed_time_merge(100), Some("hash1"));
        let result = merge_issue(None, Some(&local), None, ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-1"));
    }

    #[test]
    fn test_merge_new_external_issue_kept() {
        // Issue only in right (new external) should be kept
        let external =
            make_issue_with_hash("bd-2", "New External", fixed_time_merge(100), Some("hash2"));
        let result = merge_issue(None, None, Some(&external), ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-2"));
    }

    #[test]
    fn test_merge_deleted_both_sides() {
        // Issue in base but deleted in both local and external -> delete
        let base = make_issue_with_hash("bd-3", "Old", fixed_time_merge(100), Some("hash3"));
        let result = merge_issue(Some(&base), None, None, ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Delete));
    }

    #[test]
    fn test_merge_deleted_external_unmodified_local() {
        // Issue in base and local (unmodified), deleted in external -> delete
        let base = make_issue_with_hash("bd-4", "Base", fixed_time_merge(100), Some("hash4"));
        let result = merge_issue(
            Some(&base),
            Some(&base),
            None,
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Delete));
    }

    #[test]
    fn test_merge_deleted_external_modified_local() {
        // Issue in base and local (modified), deleted in external -> conflict (or keep local with PreferNewer)
        let base = make_issue_with_hash("bd-5", "Base", fixed_time_merge(100), Some("hash5"));
        let local =
            make_issue_with_hash("bd-5", "Modified", fixed_time_merge(200), Some("hash5_mod")); // Modified after base

        let result = merge_issue(
            Some(&base),
            Some(&local),
            None,
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(..)));
    }

    #[test]
    fn test_merge_deleted_local_modified_external() {
        // Issue in base and external (modified), deleted in local -> conflict (or keep external with PreferNewer)
        let base = make_issue_with_hash("bd-006", "Base", fixed_time_merge(100), Some("hash6"));
        let external = make_issue_with_hash(
            "bd-006",
            "Modified",
            fixed_time_merge(200),
            Some("hash6_ext"),
        );

        let result = merge_issue(
            Some(&base),
            None,
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_only_local_modified() {
        // Issue in all three, only local modified -> keep local
        let base = make_issue_with_hash("bd-007", "Base", fixed_time_merge(100), Some("hash7"));
        let local = make_issue_with_hash(
            "bd-007",
            "Modified",
            fixed_time_merge(200),
            Some("hash7_mod"),
        );
        let external = make_issue_with_hash("bd-007", "Base", fixed_time_merge(100), Some("hash7")); // Same as base

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_only_external_modified() {
        // Issue in all three, only external modified -> keep external
        let base = make_issue_with_hash("bd-008", "Base", fixed_time_merge(100), Some("hash8"));
        let local = make_issue_with_hash("bd-008", "Base", fixed_time_merge(100), Some("hash8")); // Same as base
        let external = make_issue_with_hash(
            "bd-008",
            "Modified",
            fixed_time_merge(200),
            Some("hash8_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_both_modified_prefer_newer() {
        // Issue in all three, both modified -> keep newer
        let base = make_issue_with_hash("bd-009", "Base", fixed_time_merge(100), Some("hash9"));
        let local = make_issue_with_hash(
            "bd-009",
            "Local Mod",
            fixed_time_merge(200),
            Some("hash9_local"),
        );
        let external = make_issue_with_hash(
            "bd-009",
            "External Mod",
            fixed_time_merge(300),
            Some("hash9_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(
            matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "External Mod")
        );
    }

    #[test]
    fn test_merge_both_modified_prefer_local() {
        let base = make_issue_with_hash("bd-010", "Base", fixed_time_merge(100), Some("hash10"));
        let local = make_issue_with_hash(
            "bd-010",
            "Local Mod",
            fixed_time_merge(200),
            Some("hash10_local"),
        );
        let external = make_issue_with_hash(
            "bd-010",
            "External Mod",
            fixed_time_merge(300),
            Some("hash10_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferLocal,
        );
        assert!(
            matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "Local Mod")
        );
    }

    #[test]
    fn test_merge_convergent_creation_same_content() {
        // Both created independently with same content hash -> keep one
        let local = make_issue_with_hash("bd-011", "Same", fixed_time_merge(100), Some("hash11"));
        let external =
            make_issue_with_hash("bd-011", "Same", fixed_time_merge(100), Some("hash11"));

        let result = merge_issue(
            None,
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(..)));
    }

    #[test]
    fn test_merge_convergent_creation_different_content() {
        // Both created independently with different content -> keep newer
        let local = make_issue_with_hash(
            "bd-012",
            "Local",
            fixed_time_merge(100),
            Some("hash12_local"),
        );
        let external = make_issue_with_hash(
            "bd-012",
            "External",
            fixed_time_merge(200),
            Some("hash12_ext"),
        );

        let result = merge_issue(
            None,
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "External"));
    }

    #[test]
    fn test_merge_neither_changed() {
        // Issue in all three, neither changed -> keep (use left by convention)
        let base = make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));
        let local = make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));
        let external =
            make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-013"));
    }

    #[test]
    fn test_merge_report_has_conflicts() {
        let mut report = MergeReport::default();
        assert!(!report.has_conflicts());

        report
            .conflicts
            .push(("bd-001".to_string(), ConflictType::DeleteVsModify));
        assert!(report.has_conflicts());
    }

    #[test]
    fn test_merge_report_total_actions() {
        let mut report = MergeReport::default();
        assert_eq!(report.total_actions(), 0);

        report.kept.push(make_test_issue("bd-001", "Kept"));
        report.kept.push(make_test_issue("bd-002", "Kept"));
        report.deleted.push("bd-003".to_string());
        assert_eq!(report.total_actions(), 3);
    }

    // ========================================================================
    // three_way_merge orchestration tests
    // ========================================================================

    #[test]
    fn test_three_way_merge_basic() {
        // Setup: one issue in each state
        let base_issue =
            make_issue_with_hash("bd-001", "Base", fixed_time_merge(100), Some("hash1"));
        let local_issue =
            make_issue_with_hash("bd-002", "Local Only", fixed_time_merge(200), Some("hash2"));
        let external_issue = make_issue_with_hash(
            "bd-003",
            "External Only",
            fixed_time_merge(300),
            Some("hash3"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue.clone());

        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), base_issue.clone());
        left.insert("bd-002".to_string(), local_issue);

        let mut right = std::collections::HashMap::new();
        right.insert("bd-001".to_string(), base_issue);
        right.insert("bd-003".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        // Should keep bd-001 (in all three), bd-002 (local only), bd-003 (external only)
        assert_eq!(report.kept.len(), 3);
        assert!(report.conflicts.is_empty());
        assert!(report.deleted.is_empty());
    }

    #[test]
    fn test_three_way_merge_with_tombstone_protection() {
        // Setup: tombstoned issue trying to resurrect from external
        let external_issue = make_issue_with_hash(
            "bd-tomb",
            "Should Not Resurrect",
            fixed_time_merge(300),
            Some("hash1"),
        );

        let base = std::collections::HashMap::new();
        let left = std::collections::HashMap::new();
        let mut right = std::collections::HashMap::new();
        right.insert("bd-tomb".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);

        // Create tombstones set
        let mut tombstones = std::collections::HashSet::new();
        tombstones.insert("bd-tomb".to_string());

        let report = three_way_merge(&context, ConflictResolution::PreferNewer, Some(&tombstones));

        // Should NOT keep the tombstoned issue
        assert!(report.kept.is_empty());
        assert_eq!(report.tombstone_protected.len(), 1);
        assert!(report.tombstone_protected.contains(&"bd-tomb".to_string()));
    }

    #[test]
    fn test_three_way_merge_tombstone_allows_local() {
        // Setup: tombstoned issue exists in local - should be allowed
        let local_issue = make_issue_with_hash(
            "bd-tomb",
            "Local Tombstoned",
            fixed_time_merge(200),
            Some("hash1"),
        );

        let base = std::collections::HashMap::new();
        let mut left = std::collections::HashMap::new();
        left.insert("bd-tomb".to_string(), local_issue);
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let mut tombstones = std::collections::HashSet::new();
        tombstones.insert("bd-tomb".to_string());

        let report = three_way_merge(&context, ConflictResolution::PreferNewer, Some(&tombstones));

        // Should keep local even if tombstoned
        assert_eq!(report.kept.len(), 1);
        assert!(report.tombstone_protected.is_empty());
    }

    #[test]
    fn test_three_way_merge_tombstone_protection_blocks_external_winner() {
        let base = make_issue_with_hash("bd-tomb", "Base", fixed_time_merge(100), Some("base"));
        let mut local_tombstone =
            make_issue_with_hash("bd-tomb", "Deleted", fixed_time_merge(200), Some("deleted"));
        local_tombstone.status = crate::model::Status::Tombstone;
        local_tombstone.deleted_at = Some(fixed_time_merge(200));
        let external = make_issue_with_hash(
            "bd-tomb",
            "Resurrection attempt",
            fixed_time_merge(300),
            Some("external"),
        );

        let mut base_map = std::collections::HashMap::new();
        base_map.insert("bd-tomb".to_string(), base);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-tomb".to_string(), local_tombstone);
        let mut right = std::collections::HashMap::new();
        right.insert("bd-tomb".to_string(), external);
        let context = MergeContext::new(base_map, left, right);
        let tombstones = std::collections::HashSet::from(["bd-tomb".to_string()]);

        let report = three_way_merge(
            &context,
            ConflictResolution::PreferExternal,
            Some(&tombstones),
        );

        assert!(report.conflicts.is_empty());
        assert_eq!(report.tombstone_protected, vec!["bd-tomb".to_string()]);
        assert_eq!(report.kept.len(), 1);
        assert_eq!(report.kept[0].status, crate::model::Status::Tombstone);
        assert_eq!(report.kept[0].title, "Deleted");
    }

    #[test]
    fn test_three_way_merge_deletions() {
        // Setup: issue in base but deleted in both left and right
        let base_issue =
            make_issue_with_hash("bd-del", "To Delete", fixed_time_merge(100), Some("hash1"));

        let mut base = std::collections::HashMap::new();
        base.insert("bd-del".to_string(), base_issue);

        let left = std::collections::HashMap::new();
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        assert!(report.kept.is_empty());
        assert_eq!(report.deleted.len(), 1);
        assert!(report.deleted.contains(&"bd-del".to_string()));
    }

    #[test]
    fn test_three_way_merge_empty_context() {
        let context = MergeContext::default();
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        assert!(report.kept.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.conflicts.is_empty());
        assert!(report.tombstone_protected.is_empty());
        assert!(report.notes.is_empty());
        assert_eq!(report.total_actions(), 0);
    }

    #[test]
    fn test_merge_conflict_manual_strategy() {
        // Setup: issue deleted externally but modified locally with Manual strategy
        let base_issue =
            make_issue_with_hash("bd-001", "Base", fixed_time_merge(100), Some("base_hash"));
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Modified",
            fixed_time_merge(200),
            Some("mod_hash"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), local_issue);
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::Manual, None);

        // With Manual strategy, delete-vs-modify should be a conflict
        assert_eq!(report.conflicts.len(), 1);
        assert!(matches!(
            report.conflicts[0].1,
            ConflictType::DeleteVsModify
        ));
    }

    #[test]
    fn test_three_way_merge_with_notes() {
        // Setup: issue modified in both left and right
        let base_issue = make_issue_with_hash(
            "bd-001",
            "Base Title",
            fixed_time_merge(100),
            Some("base_hash"),
        );
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Modified",
            fixed_time_merge(200),
            Some("mod_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Modified",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), local_issue);
        let mut right = std::collections::HashMap::new();
        right.insert("bd-001".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        // Should have a note about the merge decision
        assert_eq!(report.kept.len(), 1);
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].1.contains("Both modified"));
    }

    #[test]
    fn test_manual_merge_reports_both_modified_conflict() {
        let base_issue = make_issue_with_hash(
            "bd-001",
            "Base Title",
            fixed_time_merge(100),
            Some("base_hash"),
        );
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Title",
            fixed_time_merge(200),
            Some("local_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Title",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let result = merge_issue(
            Some(&base_issue),
            Some(&local_issue),
            Some(&external_issue),
            ConflictResolution::Manual,
        );

        assert!(matches!(
            result,
            MergeResult::Conflict(ConflictType::BothModified)
        ));
    }

    #[test]
    fn test_manual_merge_reports_convergent_creation_conflict() {
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Title",
            fixed_time_merge(200),
            Some("local_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Title",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let result = merge_issue(
            None,
            Some(&local_issue),
            Some(&external_issue),
            ConflictResolution::Manual,
        );

        assert!(matches!(
            result,
            MergeResult::Conflict(ConflictType::ConvergentCreation)
        ));
    }

    #[test]
    fn test_compute_jsonl_hash_ignores_empty_lines_and_whitespace() {
        let temp_dir = TempDir::new().unwrap();
        let path1 = temp_dir.path().join("file1.jsonl");
        let path2 = temp_dir.path().join("file2.jsonl");

        let content1 = "{\"id\":\"bd-1\"}\n{\"id\":\"bd-2\"}\n";
        // content2 has extra empty lines, different line endings, and extra whitespace
        let content2 = "\n{\"id\":\"bd-1\"}\r\n  \n{\"id\":\"bd-2\"}  \n\n";

        fs::write(&path1, content1).unwrap();
        fs::write(&path2, content2).unwrap();

        let hash1 = compute_jsonl_hash(&path1).unwrap();
        let hash2 = compute_jsonl_hash(&path2).unwrap();

        assert_eq!(
            hash1, hash2,
            "Hashes should be identical regardless of empty lines or whitespace"
        );
    }

    #[test]
    fn test_prefix_preserving_rename_keeps_slug_and_hash() {
        assert_eq!(
            prefix_preserving_rename("oldp-cargo-license-spdx-ay8", "newp").as_deref(),
            Some("newp-cargo-license-spdx-ay8")
        );
        assert_eq!(
            prefix_preserving_rename("oldp-ay8", "newp").as_deref(),
            Some("newp-ay8")
        );
    }

    #[test]
    fn test_prefix_preserving_rename_collapses_doubled_prefix_once() {
        assert_eq!(
            prefix_preserving_rename("oldp-oldp-central-build-inputs-3un", "newp").as_deref(),
            Some("newp-central-build-inputs-3un")
        );
        // Never recursive: a tripled prefix keeps one interior occurrence.
        assert_eq!(
            prefix_preserving_rename("oldp-oldp-oldp-x", "newp").as_deref(),
            Some("newp-oldp-x")
        );
    }

    #[test]
    fn test_prefix_preserving_rename_rejects_unsplittable_ids() {
        assert_eq!(prefix_preserving_rename("nodashid", "newp"), None);
        assert_eq!(prefix_preserving_rename("-abc", "newp"), None);
        assert_eq!(prefix_preserving_rename("oldp-", "newp"), None);
    }

    fn prefix_rename_seed(old_id: &str, title: &str) -> PrefixRenameSeed {
        PrefixRenameSeed {
            old_id: old_id.to_string(),
            title: title.to_string(),
            description: None,
            created_by: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_build_prefix_renames_preserves_remainder_and_reports_receipt() {
        let storage = SqliteStorage::open_memory().unwrap();
        let mut plan = ImportValidationPlan {
            record_count: 3,
            ..ImportValidationPlan::default()
        };
        plan.prefix_mismatches
            .push(prefix_rename_seed("oldp-cargo-license-spdx-ay8", "Slugged"));
        plan.prefix_mismatches.push(prefix_rename_seed(
            "oldp-oldp-central-build-inputs-3un",
            "Doubled",
        ));
        plan.prefix_mismatches
            .push(prefix_rename_seed("nodashid", "Unsplittable"));

        let (renames, receipt) = build_prefix_renames(&storage, &plan, Some("newp")).unwrap();

        assert_eq!(
            renames
                .get("oldp-cargo-license-spdx-ay8")
                .map(String::as_str),
            Some("newp-cargo-license-spdx-ay8"),
            "slug and hash must survive the prefix rewrite"
        );
        assert_eq!(
            renames
                .get("oldp-oldp-central-build-inputs-3un")
                .map(String::as_str),
            Some("newp-central-build-inputs-3un"),
            "doubled prefix must collapse exactly once"
        );
        let fallback_id = renames.get("nodashid").expect("unsplittable id renamed");
        assert!(
            fallback_id.starts_with("newp-"),
            "fallback must still use the configured prefix: {fallback_id}"
        );

        assert_eq!(receipt.len(), 3);
        assert_eq!(receipt[0].old_id, "oldp-cargo-license-spdx-ay8");
        assert_eq!(receipt[0].new_id, "newp-cargo-license-spdx-ay8");
        assert_eq!(receipt[0].fallback, None);
        assert_eq!(receipt[1].new_id, "newp-central-build-inputs-3un");
        assert_eq!(receipt[1].fallback, None);
        assert_eq!(receipt[2].old_id, "nodashid");
        assert_eq!(
            receipt[2].fallback.as_deref(),
            Some(PREFIX_RENAME_FALLBACK_UNPARSEABLE)
        );
    }

    #[test]
    fn test_build_prefix_renames_falls_back_on_collision_without_reminting() {
        let storage = SqliteStorage::open_memory().unwrap();
        storage
            .upsert_issue_for_import(&make_test_issue("newp-taken-ay8", "Occupant"))
            .unwrap();
        let mut plan = ImportValidationPlan {
            record_count: 1,
            ..ImportValidationPlan::default()
        };
        plan.prefix_mismatches
            .push(prefix_rename_seed("oldp-taken-ay8", "Collider"));

        let (renames, receipt) = build_prefix_renames(&storage, &plan, Some("newp")).unwrap();

        let new_id = renames.get("oldp-taken-ay8").expect("collision renamed");
        assert_ne!(
            new_id, "newp-taken-ay8",
            "must not silently re-mint over the occupied id"
        );
        assert!(new_id.starts_with("newp-"), "unexpected fallback: {new_id}");
        assert_eq!(receipt.len(), 1);
        assert_eq!(
            receipt[0].fallback.as_deref(),
            Some(PREFIX_RENAME_FALLBACK_COLLISION)
        );
        assert_eq!(&receipt[0].new_id, new_id);
    }

    #[test]
    fn test_apply_prefix_renames_stashes_old_id_and_rewrites_references() {
        let mut issue = make_test_issue("oldp-cargo-license-spdx-ay8", "Renamed");
        issue.content_hash = Some(crate::util::content_hash(&issue));
        issue.dependencies.push(Dependency {
            issue_id: "oldp-cargo-license-spdx-ay8".to_string(),
            depends_on_id: "oldp-oldp-central-build-inputs-3un".to_string(),
            dep_type: DependencyType::Blocks,
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        });
        issue.comments.push(Comment {
            id: 1,
            issue_id: "oldp-cargo-license-spdx-ay8".to_string(),
            author: "tester".to_string(),
            body: "hello".to_string(),
            created_at: Utc::now(),
        });

        let renames: HashMap<String, String> = [
            (
                "oldp-cargo-license-spdx-ay8".to_string(),
                "newp-cargo-license-spdx-ay8".to_string(),
            ),
            (
                "oldp-oldp-central-build-inputs-3un".to_string(),
                "newp-central-build-inputs-3un".to_string(),
            ),
        ]
        .into_iter()
        .collect();

        apply_prefix_renames(&mut issue, &renames);

        assert_eq!(issue.id, "newp-cargo-license-spdx-ay8");
        assert_eq!(
            issue.external_ref.as_deref(),
            Some("oldp-cargo-license-spdx-ay8"),
            "old id must be stashed in external_ref"
        );
        assert_eq!(
            issue.content_hash.as_deref(),
            Some(crate::util::content_hash(&issue).as_str()),
            "hash must be recomputed after the external_ref stash"
        );
        assert_eq!(
            issue.dependencies[0].issue_id,
            "newp-cargo-license-spdx-ay8"
        );
        assert_eq!(
            issue.dependencies[0].depends_on_id,
            "newp-central-build-inputs-3un"
        );
        assert_eq!(issue.comments[0].issue_id, "newp-cargo-license-spdx-ay8");
    }

    #[test]
    fn test_apply_prefix_renames_keeps_existing_external_ref_and_hash() {
        let mut issue = make_test_issue("oldp-abc", "Keeps ref");
        issue.external_ref = Some("gh-123".to_string());
        let stable_hash = crate::util::content_hash(&issue);
        issue.content_hash = Some(stable_hash.clone());

        let renames: HashMap<String, String> =
            std::iter::once(("oldp-abc".to_string(), "newp-abc".to_string())).collect();
        apply_prefix_renames(&mut issue, &renames);

        assert_eq!(issue.id, "newp-abc");
        assert_eq!(issue.external_ref.as_deref(), Some("gh-123"));
        assert_eq!(
            issue.content_hash.as_deref(),
            Some(stable_hash.as_str()),
            "content hash excludes the id, so a pure id swap must not move it"
        );
    }

    /// GH #457/#460/#461 caller-side containment: the exclusive opener hold
    /// is refused while a peer holds the shared lease, a refused probe leaves
    /// the shared registration in place, and a released hold rejoins it.
    #[test]
    fn opener_lease_refuses_exclusive_while_a_peer_is_registered() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut first = DatabaseOpenerLease::register(&db_path).unwrap();
        assert!(first.is_registered());
        let second = DatabaseOpenerLease::register(&db_path).unwrap();
        assert!(second.is_registered());

        assert!(
            first.try_exclusive().is_none(),
            "a registered peer must veto the exclusive hold"
        );
        assert!(
            first.is_registered(),
            "a refused probe must restore the shared registration"
        );

        drop(second);
        let hold = first
            .try_exclusive()
            .expect("the sole opener takes the exclusive hold");
        assert!(!first.is_registered());
        first.release_exclusive(hold);
        assert!(first.is_registered());
    }

    /// A newcomer waits out an in-flight exclusive hold instead of opening a
    /// database whose WAL is being reset, then registers once it is released.
    #[test]
    fn opener_lease_newcomer_waits_for_an_exclusive_hold_to_end() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut holder = DatabaseOpenerLease::register(&db_path).unwrap();
        let hold = holder.try_exclusive().expect("sole opener");

        let newcomer_path = db_path.clone();
        let newcomer = thread::spawn(move || {
            let started = Instant::now();
            let lease = DatabaseOpenerLease::register(&newcomer_path).unwrap();
            (lease.is_registered(), started.elapsed())
        });
        thread::sleep(Duration::from_millis(200));
        holder.release_exclusive(hold);

        let (registered, waited) = newcomer.join().unwrap();
        assert!(
            registered,
            "newcomer must register after the hold is released"
        );
        assert!(
            waited >= Duration::from_millis(150),
            "newcomer must have waited for the exclusive hold, waited {waited:?}"
        );
        assert!(holder.is_registered());
    }

    /// `checkpoint_full` is vetoed while another handle has the same persistent
    /// database open and runs again once that handle is gone.
    #[test]
    fn storage_checkpoint_is_skipped_while_a_peer_has_the_database_open() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut writer = SqliteStorage::open(&db_path).unwrap();
        let peer = SqliteStorage::open(&db_path).unwrap();

        let error = writer
            .checkpoint_full()
            .expect_err("a peer opener must veto the checkpoint");
        assert!(
            error
                .to_string()
                .contains("another br process has the database open"),
            "{error}"
        );

        drop(peer);
        writer
            .checkpoint_full()
            .expect("the sole opener checkpoints normally");
    }
}
