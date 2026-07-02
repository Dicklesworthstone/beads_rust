//! `mutate()` — the single chokepoint for every disk write performed by
//! `br doctor --repair` (R-001).
//!
//! ## Contract
//!
//! Every disk-write under `--repair` flows through [`mutate`]. No
//! exceptions. The 8-step contract (paraphrased from
//! `references/methodology/MUTATE-CHOKEPOINT.md`):
//!
//! 1. Acquire a per-path advisory lock.
//! 2. Compute SHA-256 `before_hash` (empty hash if the file does not
//!    exist).
//! 3. Validate preconditions (path is inside [`Capabilities::write_scopes`],
//!    op is allowed, …).
//! 4. Write a verbatim backup to `<run-dir>/backups/<rel-path>`,
//!    preserving permissions + mtime, and verify byte-identical via
//!    a strict in-process `cmp -s`-equivalent.
//! 5. Plan the mutation in memory (op-specific).
//! 6. Execute atomically (write-tmp-then-rename, transaction, …).
//! 7. Compute SHA-256 `after_hash`.
//! 8. Append a JSON line to `actions.jsonl` and return [`ActionResult`].
//!
//! If any step 3-6 fails, no `actions.jsonl` line is written, no backup
//! is consumed, and the workspace is unchanged. Steps 7-8 should be
//! infallible in practice; if they fault, restoration is the caller's
//! responsibility (the verbatim backup is sitting in
//! `<run-dir>/backups/`).
//!
//! ## Forbidden ops
//!
//! There is no `Op::Delete`. Per AGENTS.md and the project's safety
//! envelope §1, file deletion is prohibited at every layer of the
//! doctor subsystem. Anything that "needs to delete" must use
//! [`Op::Rename`] to move into the per-run quarantine area
//! (`<run-dir>/quarantine/<rel-path>`). The user can review and
//! manually remove the quarantined files later — that decision is
//! theirs, not the doctor's.
//!
//! ## DB ops
//!
//! [`Op::DbExec`] runs a parameterized SQL statement against
//! `<repo>/.beads/beads.db` inside a `BEGIN IMMEDIATE` transaction.
//! Before the SQL fires, every row of every table named in
//! `affected_tables` is snapshotted as JSON to
//! `<run-dir>/backups/db/<table>__<sha8>__<ns>[__<collision>].json`
//! (where `sha8` is the first 8 hex chars of `sha256(<predicate>)`,
//! `<ns>` is a zero-padded wall-clock nanosecond counter, and the
//! collision suffix is added only if needed). Snapshot files are opened
//! with `create_new` so multiple calls within a single run cannot
//! clobber each other. On any error the transaction is rolled back and
//! **no** `actions.jsonl` line is written.
//!
//! [`Op::DbMigrate`] runs a versioned schema migration end-to-end:
//! verbatim `beads.db.pre-migrate` snapshot, `PRAGMA user_version ==
//! from` precondition gate, then
//! [`crate::storage::schema::run_migrations_atomic`]. Failure restores
//! the DB file from the snapshot. `beads_rust-folg` closed when the
//! public hook landed.
//!
//! ## Crate-internal-only
//!
//! WP1 keeps `mutate()` `pub(crate)` so out-of-crate callers cannot
//! bypass the doctor's invariant checks. The upcoming WP3-WP12
//! migration of existing `repair_*` functions is the only consumer.

#![allow(dead_code)] // WP1 foundation; consumed by WP3-WP12.
#![allow(clippy::similar_names)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::BeadsError;
use crate::util::hex_encode;

/// Empty-input SHA-256 (i.e., `sha256("")`), prefixed with `sha256:`.
///
/// Used as the "did not exist" sentinel for missing files in
/// [`ActionRecord::before_hash`].
const SHA256_EMPTY_PREFIXED: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[cfg(unix)]
fn metadata_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn metadata_mode(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn apply_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn copy_source_permissions(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::metadata(src)?;
    apply_mode(dst, metadata_mode(&meta))
}

#[cfg(not(unix))]
fn copy_source_permissions(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, meta.permissions())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let resolved_target = link
        .parent()
        .map_or_else(|| target.to_path_buf(), |parent| parent.join(target));
    if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(all(not(unix), not(windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "doctor: symlink creation is not supported on this platform",
    ))
}

/// One disk-mutating operation the chokepoint can execute.
///
/// The variants are stable — adding a new variant is fine, but
/// renaming or removing one is a breaking change for `actions.jsonl`
/// readers.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    /// Create-or-overwrite the file at `path` with `content`. The
    /// resulting file's mode is set to `mode` if `Some`, otherwise the
    /// platform default (usually `0o644`).
    WriteFile {
        #[serde(skip)]
        content: Vec<u8>,
        mode: Option<u32>,
    },
    /// Append `content` to the file at `path`. Creates the file if it
    /// does not exist.
    AppendFile {
        #[serde(skip)]
        content: Vec<u8>,
    },
    /// Rename `path` (the source) to `to` (the destination). The
    /// destination's parent is created if it does not exist. `path`
    /// and `to` must be on the same filesystem for atomicity.
    Rename { to: PathBuf },
    /// Set the mode of `path`.
    Chmod { mode: u32 },
    /// Execute `sql` against the project's `fsqlite` DB inside a
    /// `BEGIN IMMEDIATE` transaction. Before the SQL fires, every row
    /// of every table named in `affected_tables` is snapshotted as
    /// JSON under `<run-dir>/backups/db/`; on any error the
    /// transaction rolls back and no `actions.jsonl` line is written.
    DbExec {
        sql: String,
        #[serde(skip)]
        args: Vec<DbArg>,
        /// Tables whose rows must be snapshotted before the SQL runs.
        /// Empty list is allowed (no snapshot taken) — the chokepoint
        /// still records the op, but undo will not be able to revert
        /// the data change.
        #[serde(default)]
        affected_tables: Vec<String>,
        /// Optional WHERE clause (without the `WHERE` keyword) used
        /// when snapshotting. Applied to every table in
        /// `affected_tables`. `None` means "snapshot the whole table".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        affected_predicate: Option<String>,
    },
    /// Run a versioned schema migration end-to-end. WP4 + `beads_rust-folg`:
    /// the chokepoint snapshots `beads.db.pre-migrate` verbatim, verifies
    /// `PRAGMA user_version == from`, then drives
    /// [`crate::storage::schema::run_migrations_atomic`]. Failure restores
    /// the DB file from the snapshot.
    DbMigrate { from: u32, to: u32 },
    /// Replace the symlink at `path` with one pointing at `target`.
    /// Implemented atomically via tmp-symlink + rename.
    SymlinkAtomic { target: PathBuf },
}

/// Lightweight stand-in for a SQL bind value. WP4 wires this through
/// the chokepoint by converting to [`fsqlite_types::value::SqliteValue`]
/// at the SQL boundary; callers can therefore stay independent of the
/// fsqlite type stack.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DbArg {
    /// SQL `NULL`.
    Null,
    /// Signed 64-bit integer.
    I64(i64),
    /// Double-precision float.
    F64(f64),
    /// UTF-8 string.
    Text(String),
    /// Raw bytes.
    Blob(Vec<u8>),
}

impl DbArg {
    /// Convert into the `fsqlite` type system. Used at the chokepoint's
    /// SQL boundary; not exposed publicly because it leaks the
    /// underlying engine type.
    fn to_sqlite_value(&self) -> fsqlite_types::value::SqliteValue {
        use fsqlite_types::value::SqliteValue;
        match self {
            Self::Null => SqliteValue::Null,
            Self::I64(n) => SqliteValue::Integer(*n),
            Self::F64(f) => SqliteValue::Float(*f),
            Self::Text(s) => SqliteValue::Text(s.as_str().into()),
            Self::Blob(b) => SqliteValue::Blob(std::sync::Arc::from(b.as_slice())),
        }
    }
}

impl Op {
    /// Stable kebab-case name of the op for `actions.jsonl`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::WriteFile { .. } => "write_file",
            Self::AppendFile { .. } => "append_file",
            Self::Rename { .. } => "rename",
            Self::Chmod { .. } => "chmod",
            Self::DbExec { .. } => "db_exec",
            Self::DbMigrate { .. } => "db_migrate",
            Self::SymlinkAtomic { .. } => "symlink_atomic",
        }
    }
}

/// Statically-declared set of paths the doctor may write under during
/// `--repair`.
///
/// In WP1 we always include the workspace `.beads/` and the doctor
/// run-dir root `.doctor/`. WP3-WP12 may extend this when wiring
/// individual fixers.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Canonicalized prefixes that paths must start with.
    pub write_scopes: Vec<PathBuf>,
}

impl Capabilities {
    /// Build a default capabilities set rooted at `repo_root`. Includes
    /// `<repo_root>/.beads/` and `<repo_root>/.doctor/`.
    #[must_use]
    pub fn for_repo(repo_root: &Path) -> Self {
        Self {
            write_scopes: vec![repo_root.join(".beads"), repo_root.join(".doctor")],
        }
    }
}

/// Per-run state shared across every [`mutate`] call. Owned by the
/// doctor driver; the contract is single-process so the inner mutex
/// over `actions_file` is lightweight.
pub struct MutateContext {
    /// Stable identifier for this doctor run. Embedded in every
    /// `actions.jsonl` line.
    pub run_id: String,
    /// `.doctor/runs/<run-id>/` — created by [`super::run_dir`].
    pub run_dir: PathBuf,
    /// Allowed write scopes.
    pub capabilities: Capabilities,
    /// `actions.jsonl` handle. Wrapped in a `Mutex` for forward
    /// compatibility with future parallel fixers.
    pub actions_file: Mutex<std::fs::File>,
    /// Identifier for the fixer that owns the current call.
    pub fixer_id: String,
    /// Workspace root used to canonicalize paths.
    pub repo_root: PathBuf,
    /// If `true`, [`mutate`] prints a `[dry-run]` line to stderr and
    /// does not touch disk.
    pub dry_run: bool,
    /// Captured at run-start; used to compute relative timestamps in
    /// `actions.jsonl`.
    pub start_ns: u128,
}

/// Outcome of a single [`mutate`] call.
#[derive(Debug, Clone)]
pub struct ActionResult {
    /// `true` if step 6 (atomic execute) succeeded.
    pub ok: bool,
    /// `sha256:<hex>` of the file before the mutation.
    pub before_hash: String,
    /// `sha256:<hex>` of the file after the mutation. Equal to
    /// `before_hash` in dry-run mode.
    pub after_hash: String,
    /// Error message if `ok == false`.
    pub error: Option<String>,
}

/// Single line in `actions.jsonl`. Stable contract.
#[derive(Debug, Clone, Serialize)]
pub struct ActionRecord {
    /// Workspace-relative path of the target.
    pub path: String,
    /// Op name (see [`Op::name`]).
    pub op: &'static str,
    /// `sha256:<hex>`.
    pub before_hash: String,
    /// `sha256:<hex>`.
    pub after_hash: String,
    /// Nanoseconds since [`MutateContext::start_ns`].
    pub started_at_ns: u128,
    /// Nanoseconds since [`MutateContext::start_ns`].
    pub finished_at_ns: u128,
    /// Stable run identifier.
    pub run_id: String,
    /// Stable fixer identifier.
    pub fixer_id: String,
    /// Whether step 6 succeeded.
    pub ok: bool,
    /// For [`Op::Rename`] only — destination path. `doctor undo` reads
    /// this to reverse the move.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_to: Option<String>,
    /// Set if the mutation faulted and was rolled back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<bool>,
    /// Set on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// DB-specific JSONL action record appended by [`mutate_db`].
#[derive(Serialize)]
struct DbActionRecord<'a> {
    path: String,
    op: &'static str,
    before_hash: &'a str,
    after_hash: &'a str,
    started_at_ns: u128,
    finished_at_ns: u128,
    run_id: &'a str,
    fixer_id: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    affected_tables: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    affected_predicate: Option<String>,
    /// Workspace-relative paths of the JSON snapshot files written by
    /// this DbExec, one per `affected_tables` entry. Empty for
    /// non-DbExec ops. Recorded so `br doctor undo` can replay the
    /// exact snapshots that were taken before the SQL fired.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    db_snapshots: Vec<String>,
    /// `sha256:<hex>` digests of each `db_snapshots` body, in the same
    /// order. Recorded so `br doctor undo` can verify that the on-disk
    /// snapshot file has not been tampered with between the forward
    /// DbExec and the replay. Without this binding, an attacker
    /// (or a buggy out-of-band tool) editing
    /// `.doctor/runs/<run-id>/backups/db/*.json` could inject rows on
    /// undo. Empty for non-DbExec ops.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    db_snapshot_sha256: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    migrate_from: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    migrate_to: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<&'static str>,
}

struct DbMutationOutcome {
    after_hash: String,
    affected_tables: Option<String>,
    affected_predicate: Option<String>,
    db_snapshots: Vec<String>,
    /// Per-snapshot SHA-256 digests in the same order as `db_snapshots`.
    /// Empty for migrate ops (no row-level snapshots there).
    db_snapshot_sha256: Vec<String>,
}

/// Compute `sha256:<hex>` of `bytes`.
fn sha256_hex_prefixed(bytes: &[u8]) -> String {
    let h = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(&h))
}

/// Read `path` into bytes; return empty `Vec` if it does not exist.
fn read_or_empty(path: &Path) -> std::io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Best-effort canonicalization that tolerates not-yet-existing files
/// (walks up to the first existing ancestor, canonicalizes that, then
/// re-joins the missing tail). This handles renames into not-yet-created
/// quarantine subdirs.
fn canonicalize_existing_or_parent(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return path.canonicalize();
    }
    // Walk up until we find an existing ancestor.
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = path;
    loop {
        if let Some(name) = cursor.file_name() {
            tail.push(name);
        }
        match cursor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                if parent.exists() {
                    let mut canonical = parent.canonicalize()?;
                    for segment in tail.iter().rev() {
                        canonical.push(segment);
                    }
                    return Ok(canonical);
                }
                cursor = parent;
            }
            _ => {
                // No existing ancestor; canonicalize CWD as a fallback.
                let cwd = Path::new(".").canonicalize()?;
                let mut canonical = cwd;
                for segment in tail.iter().rev() {
                    canonical.push(segment);
                }
                return Ok(canonical);
            }
        }
    }
}

fn ensure_in_scope(caps: &Capabilities, path: &Path) -> Result<(), BeadsError> {
    let canonical = canonicalize_existing_or_parent(path).map_err(BeadsError::Io)?;
    for scope in &caps.write_scopes {
        // Tolerate scopes that haven't been created yet (e.g.,
        // `.doctor/` on a fresh workspace).
        let canonical_scope =
            canonicalize_existing_or_parent(scope).unwrap_or_else(|_| scope.clone());
        if canonical.starts_with(&canonical_scope) {
            return Ok(());
        }
    }
    Err(BeadsError::internal(format!(
        "doctor: path {} is outside write_scopes (refused for safety)",
        path.display()
    )))
}

fn copy_verbatim_with_perms(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst)?;
    copy_source_permissions(src, dst)?;
    Ok(())
}

/// Write the captured `bytes` (already read into memory by `mutate()`)
/// to `dst` and stamp it with the source file's mode. Unlike
/// `copy_verbatim_with_perms` this never re-reads the source — it uses
/// the in-memory bytes the chokepoint already hashed for `before_hash`.
/// `cmp_strict` is then run by the caller as a tripwire that fires if a
/// non-doctor writer touched the live file between our read and now.
fn write_verbatim_backup(src: &Path, dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dst, bytes)?;
    copy_source_permissions(src, dst)?;
    Ok(())
}

/// Strict byte-by-byte comparison of two files. Used to verify the
/// verbatim backup matches the live file before we proceed with the
/// mutation.
fn cmp_strict(a: &Path, b: &Path) -> std::io::Result<()> {
    let ba = fs::read(a)?;
    let bb = fs::read(b)?;
    if ba != bb {
        return Err(std::io::Error::other(
            "doctor: backup verify failed (cmp-strict)",
        ));
    }
    Ok(())
}

fn now_ns() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Fsync the given directory so a freshly-created entry is durable
/// across power loss. POSIX guarantees `rename(2)` atomicity but not
/// durability — the directory's data block must be flushed for the
/// new entry to survive a crash. A minority of filesystems (e.g. some
/// kernel-side tmpfs configurations) reject fsync on a directory; we
/// treat `InvalidInput` as best-effort and only propagate genuine
/// I/O faults so a perfectly successful mutate is not turned into a
/// false negative.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let d = fs::File::open(dir)?;
    match d.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(e) => Err(e),
    }
}

/// The single disk-mutation chokepoint for `br doctor --repair`.
///
/// See the module-level documentation for the 8-step contract.
///
/// # Errors
///
/// Returns [`BeadsError`] for:
/// - I/O faults during backup or atomic-write
/// - Out-of-scope paths (refused per safety envelope)
/// - DB ops that fail the precondition gate or the SQL transaction
pub fn mutate(ctx: &MutateContext, path: &Path, op: Op) -> Result<ActionResult, BeadsError> {
    // (1) Per-path advisory lock — minimal in-process mutex via the
    // actions_file lock; cross-process locking is provided by the
    // existing `acquire_routed_workspace_write_lock` upstream of the
    // doctor driver. We deliberately do NOT introduce a second OS-level
    // lockfile here; the workspace lock is already held when --repair
    // runs.

    // (2) Read once and remember whether the file existed BEFORE we do
    //     anything else. The chokepoint's whole reason to exist is
    //     consistency between `before_hash`, `backup contents`, and the
    //     pre-mutation live file; any subsequent step that re-reads the
    //     live file opens a TOCTOU window. We compute `before_hash` from
    //     the bytes we capture here, then write those exact bytes as
    //     the verbatim backup so the audit log and the backup file
    //     trivially agree.
    let before_bytes_or_missing = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(BeadsError::Io(e)),
    };
    let before_hash = match &before_bytes_or_missing {
        Some(bytes) => sha256_hex_prefixed(bytes),
        None => SHA256_EMPTY_PREFIXED.to_string(),
    };

    // (3) Preconditions — write_scopes check.
    ensure_in_scope(&ctx.capabilities, path)?;
    if let Op::Rename { to } = &op {
        ensure_in_scope(&ctx.capabilities, to)?;
    }

    // DB ops route through their dedicated handler. The chokepoint
    // contract still applies — `path` must be in scope (it is the
    // beads.db file), backups go to `<run-dir>/backups/`, and
    // `actions.jsonl` records the op exactly once.
    if matches!(op, Op::DbExec { .. } | Op::DbMigrate { .. }) {
        return mutate_db(ctx, path, &op, &before_hash);
    }
    let op_name = op.name();
    let rename_to = match &op {
        Op::Rename { to } => Some(to.to_string_lossy().into_owned()),
        _ => None,
    };

    // (4) Verbatim backup — only meaningful if the file existed. Write
    //     the exact bytes we hashed for `before_hash` so the audit log
    //     and the on-disk backup are guaranteed consistent. The
    //     belt-and-braces `cmp_strict` is preserved for the case where
    //     a non-doctor writer touches the file *after* our read; if the
    //     backup we just wrote no longer matches the live file we
    //     refuse rather than commit to a stale before_hash.
    //
    //     We also capture the live file's mode here so an Op::AppendFile
    //     can stamp the temp file with the same mode without performing
    //     a third stat(2) inside execute_atomic — that third stat opened
    //     a TOCTOU window between cmp_strict and execute_atomic where a
    //     concurrent writer could swap the file (and its mode) under us.
    let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
    let backup = ctx.run_dir.join("backups").join(rel);
    let existing_mode = if before_bytes_or_missing.is_some() {
        match fs::metadata(path) {
            Ok(meta) => Some(metadata_mode(&meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(BeadsError::Io(e)),
        }
    } else {
        None
    };
    if !ctx.dry_run
        && let Some(bytes) = before_bytes_or_missing.as_ref()
    {
        write_verbatim_backup(path, &backup, bytes).map_err(BeadsError::Io)?;
        cmp_strict(path, &backup).map_err(BeadsError::Io)?;
    }

    // (5) Plan + (6) Execute atomically.
    let started_at_ns = now_ns().saturating_sub(ctx.start_ns);
    if ctx.dry_run {
        eprintln!("[dry-run] would mutate {}: {}", path.display(), op_name);
        return Ok(ActionResult {
            ok: true,
            before_hash: before_hash.clone(),
            after_hash: before_hash,
            error: None,
        });
    }
    // Hand the captured `before_bytes` and `existing_mode` to
    // `execute_atomic` so Op::AppendFile can compute the merged contents
    // from the *hashed-and-backed-up* bytes rather than re-reading the
    // live file — closing the TOCTOU window where a concurrent writer
    // could change the live file between cmp_strict and the append's
    // own read(). The bytes are exactly what we already wrote to the
    // verbatim backup, so undo can still recover the original file.
    execute_atomic(path, op, before_bytes_or_missing.as_deref(), existing_mode)
        .map_err(BeadsError::Io)?;

    // (7) after_hash.
    let after_bytes = read_or_empty(path).map_err(BeadsError::Io)?;
    let after_exists = path.exists();
    let after_hash = if after_exists {
        sha256_hex_prefixed(&after_bytes)
    } else {
        // For Rename ops, the source is gone — record the empty hash
        // so undo can detect "this path was emptied / moved".
        SHA256_EMPTY_PREFIXED.to_string()
    };
    let finished_at_ns = now_ns().saturating_sub(ctx.start_ns);

    // (8) Record.
    let record = ActionRecord {
        path: rel.to_string_lossy().into_owned(),
        op: op_name,
        before_hash: before_hash.clone(),
        after_hash: after_hash.clone(),
        started_at_ns,
        finished_at_ns,
        run_id: ctx.run_id.clone(),
        fixer_id: ctx.fixer_id.clone(),
        ok: true,
        rename_to,
        rolled_back: None,
        error: None,
    };
    let line = serde_json::to_string(&record).map_err(BeadsError::Json)? + "\n";
    {
        let mut f = ctx.actions_file.lock().map_err(|e| {
            BeadsError::internal(format!("doctor: actions_file mutex poisoned: {e}"))
        })?;
        f.write_all(line.as_bytes()).map_err(BeadsError::Io)?;
        f.sync_data().map_err(BeadsError::Io)?;
    }

    Ok(ActionResult {
        ok: true,
        before_hash,
        after_hash,
        error: None,
    })
}

/// Routed handler for [`Op::DbExec`] and [`Op::DbMigrate`].
///
/// Implements the same 8-step contract as [`mutate`] but tailored to a
/// SQLite database file:
///
/// 1. Refuse if `path` is outside the configured write scopes (already
///    checked by the caller).
/// 2. For `DbExec`: snapshot affected rows as JSON to
///    `<run-dir>/backups/db/<table>__<sha8>__<ns>[__<collision>].json`
///    before any SQL runs.
///    For `DbMigrate`: snapshot the entire DB file verbatim to
///    `<run-dir>/backups/db/beads.db.pre-migrate`.
/// 3. Open a writable `fsqlite::Connection`, run the work inside
///    the migration hook. On any error, restore from the pre-migrate
///    snapshot and return without writing an `actions.jsonl` line.
/// 4. Compute `after_hash` from the post-COMMIT DB file SHA-256.
/// 5. Append a single `actions.jsonl` record describing the op.
#[allow(clippy::too_many_lines)]
fn mutate_db(
    ctx: &MutateContext,
    path: &Path,
    op: &Op,
    before_hash: &str,
) -> Result<ActionResult, BeadsError> {
    // Snapshot path under run-dir.
    let backups_db = ctx.run_dir.join("backups").join("db");

    // Pre-write inventory. We capture this before any disk write so a
    // crash mid-snapshot is recoverable from the JSONL log + the
    // pre-existing DB file.
    let started_at_ns = now_ns().saturating_sub(ctx.start_ns);

    if ctx.dry_run {
        eprintln!("[dry-run] would mutate {}: {}", path.display(), op.name());
        return Ok(ActionResult {
            ok: true,
            before_hash: before_hash.to_string(),
            after_hash: before_hash.to_string(),
            error: None,
        });
    }

    let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
    let op_name = op.name();

    let exec_result: Result<DbMutationOutcome, BeadsError> = match op {
        Op::DbExec {
            sql,
            args,
            affected_tables,
            affected_predicate,
        } => {
            fs::create_dir_all(&backups_db).map_err(BeadsError::Io)?;

            let predicate_for_snapshot = affected_predicate.clone();
            let tables_for_snapshot = affected_tables.clone();
            let sql_owned = sql.clone();
            let args_owned: Vec<fsqlite_types::value::SqliteValue> =
                args.iter().map(DbArg::to_sqlite_value).collect();

            run_db_exec(
                path,
                &backups_db,
                &sql_owned,
                &args_owned,
                &tables_for_snapshot,
                predicate_for_snapshot.as_deref(),
            )
            .map(|artifacts| {
                let table_summary = if tables_for_snapshot.is_empty() {
                    None
                } else {
                    Some(tables_for_snapshot.join(","))
                };
                let mut snapshot_strs = Vec::with_capacity(artifacts.len());
                let mut snapshot_hashes = Vec::with_capacity(artifacts.len());
                // The body-level sha256 is computed by `snapshot_db_table`
                // BEFORE the snapshot file is written and returned here in
                // `artifact.sha256_prefixed`. Recording it in
                // actions.jsonl is what gives `br doctor undo` a way to
                // detect snapshot-file tampering between the forward
                // DbExec and the replay — closing the round-2 follow-up
                // gap on hash-binding the snapshot to the action.
                for artifact in artifacts {
                    snapshot_strs.push(
                        artifact
                            .path
                            .strip_prefix(&ctx.repo_root)
                            .unwrap_or(&artifact.path)
                            .to_string_lossy()
                            .into_owned(),
                    );
                    snapshot_hashes.push(artifact.sha256_prefixed);
                }
                DbMutationOutcome {
                    after_hash: sha256_file_hex_prefixed(path)
                        .unwrap_or_else(|_| SHA256_EMPTY_PREFIXED.to_string()),
                    affected_tables: table_summary,
                    affected_predicate: predicate_for_snapshot,
                    db_snapshots: snapshot_strs,
                    db_snapshot_sha256: snapshot_hashes,
                }
            })
        }
        Op::DbMigrate { from, to } => {
            fs::create_dir_all(&backups_db).map_err(BeadsError::Io)?;
            run_db_migrate(path, &backups_db, *from, *to).map(|_warning| DbMutationOutcome {
                after_hash: sha256_file_hex_prefixed(path)
                    .unwrap_or_else(|_| SHA256_EMPTY_PREFIXED.to_string()),
                affected_tables: None,
                affected_predicate: None,
                db_snapshots: Vec::new(),
                db_snapshot_sha256: Vec::new(),
            })
        }
        _ => unreachable!("mutate_db only handles DB ops"),
    };

    let DbMutationOutcome {
        after_hash,
        affected_tables: db_affected_tables,
        affected_predicate: db_predicate,
        db_snapshots,
        db_snapshot_sha256,
    } = exec_result?;
    let finished_at_ns = now_ns().saturating_sub(ctx.start_ns);

    let (migrate_from, migrate_to, warning) = match op {
        Op::DbMigrate { from, to } => (Some(*from), Some(*to), None),
        _ => (None, None, None),
    };

    let record = DbActionRecord {
        path: rel.to_string_lossy().into_owned(),
        op: op_name,
        before_hash,
        after_hash: &after_hash,
        started_at_ns,
        finished_at_ns,
        run_id: &ctx.run_id,
        fixer_id: &ctx.fixer_id,
        ok: true,
        affected_tables: db_affected_tables,
        affected_predicate: db_predicate,
        db_snapshots,
        db_snapshot_sha256,
        migrate_from,
        migrate_to,
        warning,
    };
    let line = serde_json::to_string(&record).map_err(BeadsError::Json)? + "\n";
    {
        let mut f = ctx.actions_file.lock().map_err(|e| {
            BeadsError::internal(format!("doctor: actions_file mutex poisoned: {e}"))
        })?;
        f.write_all(line.as_bytes()).map_err(BeadsError::Io)?;
        f.sync_data().map_err(BeadsError::Io)?;
    }

    Ok(ActionResult {
        ok: true,
        before_hash: before_hash.to_string(),
        after_hash,
        error: None,
    })
}

/// Compute `sha256:<hex>` of the file at `path`. Returns the empty
/// sentinel if the file does not exist.
fn sha256_file_hex_prefixed(path: &Path) -> std::io::Result<String> {
    let bytes = read_or_empty(path)?;
    if bytes.is_empty() && !path.exists() {
        return Ok(SHA256_EMPTY_PREFIXED.to_string());
    }
    Ok(sha256_hex_prefixed(&bytes))
}

/// Snapshot every row of every table in `tables`, write the JSON, then
/// run `sql` with `args` inside a `BEGIN IMMEDIATE` transaction. On any
/// error, `ROLLBACK` and propagate. Returns the absolute paths of the
/// snapshot files written (one per `affected_tables` entry, in order),
/// so the caller can record them in `actions.jsonl` for `doctor undo`.
///
/// ## Single-connection contract (round-2 fresh-eyes fix)
///
/// The snapshot SELECTs run on the **same** `fsqlite::Connection` and
/// **inside** the same `BEGIN IMMEDIATE` transaction as the mutating
/// SQL. This closes the dual-connection race window that existed when
/// snapshotting used a separate connection: an external writer could
/// have committed a change between `read_conn.close()` and the
/// `BEGIN IMMEDIATE` on the writer connection, and the snapshot would
/// have missed that data. Restoring such a snapshot during `br doctor
/// undo` would silently destroy the concurrent writer's commit.
///
/// Holding the reserved/exclusive lock for the entire snapshot-then-
/// mutate window guarantees: (a) the snapshot reflects exactly the
/// rows the mutating SQL is about to operate on, and (b) no other
/// writer can sneak in between snapshot and mutation.
fn run_db_exec(
    db_path: &Path,
    backups_db: &Path,
    sql: &str,
    args: &[fsqlite_types::value::SqliteValue],
    affected_tables: &[String],
    affected_predicate: Option<&str>,
) -> Result<Vec<DbSnapshotArtifact>, BeadsError> {
    use fsqlite::Connection;

    // Pre-flight identifier validation so we fail fast before opening
    // the DB connection. Mirrors the protection on the SELECT path.
    for table in affected_tables {
        validate_identifier(table)?;
    }

    let mut snapshot_artifacts: Vec<DbSnapshotArtifact> = Vec::with_capacity(affected_tables.len());

    // Single connection for the entire snapshot+mutate transaction.
    // Acquire BEGIN IMMEDIATE *before* the SELECTs so the writer lock
    // is held for the snapshot read as well as the mutating SQL.
    let conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    if let Err(e) = conn.execute("BEGIN IMMEDIATE") {
        let _ = conn.close();
        return Err(e.into());
    }

    // Snapshot every affected table inside the transaction. Any error
    // in this loop must ROLLBACK + scrub the partial snapshot files we
    // already wrote — they have no actions.jsonl entry referencing them.
    for table in affected_tables {
        let artifact = match snapshot_db_table(&conn, backups_db, table, affected_predicate) {
            Ok(a) => a,
            Err(e) => {
                let _ = conn.execute("ROLLBACK");
                let _ = conn.close();
                scrub_orphan_snapshots(
                    &snapshot_artifacts
                        .iter()
                        .map(|a| a.path.clone())
                        .collect::<Vec<_>>(),
                );
                return Err(e);
            }
        };
        snapshot_artifacts.push(artifact);
    }

    // Run the mutating SQL on the same connection, still inside
    // BEGIN IMMEDIATE. Any error rolls back AND scrubs the orphan
    // snapshots — a snapshot with no corresponding actions.jsonl entry
    // is dead weight that would only confuse forensic tooling.
    let exec_outcome = if args.is_empty() {
        conn.execute(sql).map(|_| ())
    } else {
        conn.execute_with_params(sql, args).map(|_| ())
    };

    match exec_outcome {
        Ok(()) => {
            if let Err(e) = conn.execute("COMMIT") {
                let _ = conn.execute("ROLLBACK");
                let _ = conn.close();
                scrub_orphan_snapshots(
                    &snapshot_artifacts
                        .iter()
                        .map(|a| a.path.clone())
                        .collect::<Vec<_>>(),
                );
                return Err(e.into());
            }
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK");
            let _ = conn.close();
            scrub_orphan_snapshots(
                &snapshot_artifacts
                    .iter()
                    .map(|a| a.path.clone())
                    .collect::<Vec<_>>(),
            );
            return Err(e.into());
        }
    }
    let _ = conn.close();
    Ok(snapshot_artifacts)
}

/// Returned by [`snapshot_db_table`] / [`run_db_exec`]. Pairs the
/// snapshot file's on-disk path with the `sha256:<hex>` digest of the
/// body that was just written. The digest is recorded in
/// `actions.jsonl` and re-verified by `br doctor undo` to detect
/// tampering of the snapshot file between the forward DbExec and the
/// replay — closing the round-2 follow-up gap where an attacker editing
/// the snapshot body could inject rows on undo.
#[derive(Debug, Clone)]
pub(crate) struct DbSnapshotArtifact {
    pub path: PathBuf,
    pub sha256_prefixed: String,
}

fn snapshot_db_table(
    conn: &fsqlite::Connection,
    backups_db: &Path,
    table: &str,
    affected_predicate: Option<&str>,
) -> Result<DbSnapshotArtifact, BeadsError> {
    let predicate = affected_predicate.unwrap_or("").trim();
    let select_sql = if predicate.is_empty() {
        format!("SELECT * FROM {table}")
    } else {
        format!("SELECT * FROM {table} WHERE {predicate}")
    };

    let column_names = collect_column_names(conn, table)?;
    let stmt = conn.prepare(&select_sql)?;
    let rows = stmt.query()?;
    let mut json_rows: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut obj = serde_json::Map::new();
        for (i, name) in column_names.iter().enumerate() {
            let val = row
                .get(i)
                .cloned()
                .unwrap_or(fsqlite_types::value::SqliteValue::Null);
            obj.insert(name.clone(), sqlite_value_to_json(&val));
        }
        json_rows.push(serde_json::Value::Object(obj));
    }

    let mut hasher = Sha256::new();
    hasher.update(predicate.as_bytes());
    let predicate_hash = &hex_encode(&hasher.finalize())[..8];
    let snapshot_envelope = serde_json::json!({
        "schema_version": "br.doctor.db_snapshot.v1",
        "table": table,
        "predicate": affected_predicate,
        "columns": column_names,
        "rows": json_rows,
    });
    let body = serde_json::to_vec_pretty(&snapshot_envelope).map_err(BeadsError::Json)?;
    let body_sha256 = sha256_hex_prefixed(&body);
    let path = write_unique_db_snapshot(backups_db, table, predicate_hash, &body)
        .map_err(BeadsError::Io)?;
    Ok(DbSnapshotArtifact {
        path,
        sha256_prefixed: body_sha256,
    })
}

/// Best-effort removal of snapshot files that were written before a
/// failing SQL statement aborted the chokepoint. The chokepoint never
/// records an `actions.jsonl` line for a rolled-back DbExec, so leaving
/// the snapshots on disk would create dangling artifacts that the undo
/// path cannot reach. Failures here are intentionally swallowed: the
/// caller is already returning the underlying SQL error.
fn scrub_orphan_snapshots(snapshot_paths: &[PathBuf]) {
    for path in snapshot_paths {
        let _ = fs::remove_file(path);
    }
}

fn write_unique_db_snapshot(
    backups_db: &Path,
    table: &str,
    predicate_hash: &str,
    body: &[u8],
) -> std::io::Result<PathBuf> {
    write_unique_db_snapshot_with_stamp(backups_db, table, predicate_hash, now_ns(), body)
}

fn write_unique_db_snapshot_with_stamp(
    backups_db: &Path,
    table: &str,
    predicate_hash: &str,
    stamp: u128,
    body: &[u8],
) -> std::io::Result<PathBuf> {
    for collision in 0..=999_u16 {
        let suffix = if collision == 0 {
            String::new()
        } else {
            format!("__{collision:03}")
        };
        let path = backups_db.join(format!(
            "{table}__{predicate_hash}__{stamp:020}{suffix}.json"
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(body)?;
                file.sync_data()?;
                return Ok(path);
            }
            Err(err) => {
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(err);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "doctor: exhausted DB snapshot collision suffixes for {table}__{predicate_hash}__{stamp:020}"
        ),
    ))
}

/// Validate that an identifier consists only of `[A-Za-z0-9_]` so it
/// cannot be used to inject SQL when interpolated into a `SELECT * FROM`
/// statement. Rejects empty strings.
fn validate_identifier(ident: &str) -> Result<(), BeadsError> {
    if ident.is_empty() || !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(BeadsError::internal(format!(
            "doctor: invalid SQL identifier '{ident}' (must be ASCII alphanumeric + underscore)"
        )));
    }
    Ok(())
}

/// Resolve the column-name vector for `table` via `PRAGMA
/// table_info`. Returns an error if the table does not exist.
fn collect_column_names(
    conn: &fsqlite::Connection,
    table: &str,
) -> Result<Vec<String>, BeadsError> {
    use fsqlite_types::value::SqliteValue;
    validate_identifier(table)?;
    let rows = conn.query(&format!("PRAGMA table_info({table})"))?;
    if rows.is_empty() {
        return Err(BeadsError::internal(format!(
            "doctor: table '{table}' has no columns (does it exist?)"
        )));
    }
    let mut names = Vec::with_capacity(rows.len());
    // PRAGMA table_info: cid, name, type, notnull, dflt_value, pk
    for row in &rows {
        if let Some(SqliteValue::Text(name)) = row.get(1) {
            names.push(name.to_string());
        } else {
            return Err(BeadsError::internal(format!(
                "doctor: PRAGMA table_info({table}) returned non-text column name"
            )));
        }
    }
    Ok(names)
}

/// JSON-encode a single `SqliteValue`. NULL→null, Integer→number,
/// Float→number, Text→string, Blob→`{"$blob_b64": "..."}` to keep the
/// JSON faithful.
fn sqlite_value_to_json(val: &fsqlite_types::value::SqliteValue) -> serde_json::Value {
    use fsqlite_types::value::SqliteValue;
    match val {
        SqliteValue::Null => serde_json::Value::Null,
        SqliteValue::Integer(n) => serde_json::Value::from(*n),
        SqliteValue::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        SqliteValue::Text(s) => serde_json::Value::String(s.to_string()),
        SqliteValue::Blob(b) => {
            // Hex-encode rather than base64 so the snapshot has no
            // additional dependency on a base64 crate. Restore is via
            // hex-decode → SqliteValue::Blob.
            serde_json::json!({ "$blob_hex": hex_encode(b.as_ref()) })
        }
    }
}

/// Drive an [`Op::DbMigrate`] end-to-end. Order of operations:
///
/// 1. Snapshot the DB file verbatim to `backups/db/beads.db.pre-migrate`
///    BEFORE opening any connection (fsqlite can dirty header counters
///    even on read-only PRAGMAs).
/// 2. Verify `PRAGMA user_version == from` and refuse with an internal
///    error otherwise.
/// 3. Call [`crate::storage::schema::run_migrations_atomic`] which
///    drives the migration steps and stamps `PRAGMA user_version = to`.
/// 4. On any failure, restore the DB file from the snapshot before
///    returning the error.
///
/// Returns `Ok(None)` on success (no warning attached to the
/// actions.jsonl record). Returns `Err` on any failure.
fn run_db_migrate(
    db_path: &Path,
    backups_db: &Path,
    from: u32,
    to: u32,
) -> Result<Option<()>, BeadsError> {
    use fsqlite::Connection;
    use fsqlite_types::value::SqliteValue;

    if to <= from {
        return Err(BeadsError::internal(format!(
            "doctor: db migrate refused — to ({to}) must be > from ({from})"
        )));
    }

    // (1) Snapshot the DB file *before* we open any connection. Opening
    //     a fsqlite connection can dirty header counters even on a
    //     read-only PRAGMA query; snapshotting first keeps the
    //     pre-migrate file byte-identical to the on-disk state callers
    //     observed before invoking the chokepoint.
    let snapshot_path = backups_db.join("beads.db.pre-migrate");
    if let Some(parent) = snapshot_path.parent() {
        fs::create_dir_all(parent).map_err(BeadsError::Io)?;
    }
    fs::copy(db_path, &snapshot_path).map_err(BeadsError::Io)?;
    copy_source_permissions(db_path, &snapshot_path).map_err(BeadsError::Io)?;

    // (2) Verify PRAGMA user_version matches `from`. If the precondition
    //     gate fails we discard the snapshot and refuse — leaving no
    //     backup behind keeps undo from trying to "restore" a no-op.
    let conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let row = conn.query_row("PRAGMA user_version")?;
    let current = row
        .get(0)
        .and_then(|v| match v {
            SqliteValue::Integer(n) => u32::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);
    let _ = conn.close();
    if current != from {
        // Discard the now-orphan snapshot so it can't masquerade as a
        // recoverable backup of a never-mutated DB.
        let _ = fs::remove_file(&snapshot_path);
        return Err(BeadsError::internal(format!(
            "doctor: db migrate refused — user_version mismatch (expected {from}, got {current})"
        )));
    }

    // (3) Drive the actual DDL via the new public hook
    //     `crate::storage::schema::run_migrations_atomic`, which stamps
    //     `PRAGMA user_version = to`. On any failure the chokepoint's
    //     recovery path can restore from the pre-migrate snapshot we
    //     wrote in step (1).
    let migrate_conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let migration_result = crate::storage::schema::run_migrations_atomic(&migrate_conn, from, to);
    let _ = migrate_conn.close();
    match migration_result {
        Ok(()) => Ok(None),
        Err(err) => {
            // The atomic wrapper already attempted ROLLBACK. Best-effort
            // restore from the pre-migrate snapshot so the live DB is
            // in a known state for callers that don't immediately invoke
            // `br doctor undo`. The chokepoint's actions.jsonl line is
            // still written by the caller — recording the failed attempt
            // is itself part of the audit contract.
            if let Err(restore_err) = fs::copy(&snapshot_path, db_path) {
                return Err(BeadsError::internal(format!(
                    "doctor: db migrate failed ({err}); restore from snapshot also failed: {restore_err}"
                )));
            }
            Err(err)
        }
    }
}

/// Execute the planned op atomically. File-based ops use
/// `tempfile::NamedTempFile::persist` (i.e., `rename(2)`); the temp
/// file lives in the **same directory** as the target so cross-FS
/// rename never breaks atomicity. After every `rename(2)` we fsync
/// the containing directory so the rename itself is durable across
/// power loss — POSIX does not guarantee directory updates land just
/// because the file's contents were synced.
///
/// `existing_bytes` / `existing_mode` are supplied by the caller from
/// the snapshot captured at step 2 of the chokepoint contract. They
/// must reflect the bytes that were just hashed for `before_hash` and
/// just written to the verbatim backup — never a fresh read of the
/// live file. Re-reading the live file here would re-open the TOCTOU
/// window the chokepoint exists to close (a concurrent writer could
/// commit a change between cmp_strict and the re-read, leaving the
/// merged contents based on data that was never backed up).
fn execute_atomic(
    path: &Path,
    op: Op,
    existing_bytes: Option<&[u8]>,
    existing_mode: Option<u32>,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    match op {
        Op::WriteFile { content, mode } => {
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            tmp.write_all(&content)?;
            tmp.as_file().sync_data()?;
            apply_mode(tmp.path(), mode.unwrap_or(0o644))?;
            tmp.persist(path)
                .map_err(|e| std::io::Error::other(e.error.to_string()))?;
            fsync_dir(parent)?;
        }
        Op::AppendFile { content } => {
            // Crash-safety: a raw `O_APPEND` write leaves a torn line
            // on the file if we crash mid-write. The chokepoint
            // contract is "atomic or not at all", so we materialize
            // the new contents (existing + new) in a tmpfile, fsync,
            // and rename(2). This costs an extra read but matches
            // the WriteFile guarantee.
            //
            // TOCTOU-fix: use the in-memory bytes the chokepoint
            // already hashed and backed up at step 2. Re-reading the
            // live file here would let a concurrent writer's commit
            // become invisible — we'd write `<backed-up-bytes>
            // + content` over their fresh write, with no record in
            // the verbatim backup that their bytes ever existed.
            let existing: &[u8] = existing_bytes.unwrap_or(&[]);
            let mut buf = Vec::with_capacity(existing.len().saturating_add(content.len()));
            buf.extend_from_slice(existing);
            buf.extend_from_slice(&content);
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            tmp.write_all(&buf)?;
            tmp.as_file().sync_data()?;
            // Preserve the file's mode captured at step 2. A fresh
            // metadata() call here would re-open the same TOCTOU
            // window; default to 0o644 if the file did not exist
            // at backup time.
            let mode = existing_mode.unwrap_or(0o644);
            apply_mode(tmp.path(), mode)?;
            tmp.persist(path)
                .map_err(|e| std::io::Error::other(e.error.to_string()))?;
            fsync_dir(parent)?;
        }
        Op::Rename { to } => {
            if let Some(p) = to.parent() {
                fs::create_dir_all(p)?;
            }
            fs::rename(path, &to)?;
            // Fsync both the source and destination directories so
            // the unlink-from-source and link-into-dest are both
            // durable. POSIX rename atomicity guarantees the rename
            // is observed atomically by other readers, but durability
            // across power loss requires an explicit dirsync.
            fsync_dir(parent)?;
            if let Some(dest_parent) = to.parent()
                && dest_parent != parent
            {
                fsync_dir(dest_parent)?;
            }
        }
        Op::Chmod { mode } => {
            apply_mode(path, mode)?;
        }
        Op::SymlinkAtomic { target } => {
            let basename = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "target".to_string());
            let tmp = path.with_file_name(format!(
                ".{}.doctor-symlink-tmp.{}.{}",
                basename,
                std::process::id(),
                now_ns()
            ));
            // Best-effort scrub of stale tmp from a crashed run.
            // We can't `is_ok() then remove_file` because that opens a
            // TOCTOU window where another process could delete the
            // symlink between our check and our remove. Just attempt
            // the remove unconditionally and ignore NotFound.
            match fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            create_symlink(&target, &tmp)?;
            fs::rename(&tmp, path)?;
            fsync_dir(parent)?;
        }
        Op::DbExec { .. } | Op::DbMigrate { .. } => {
            // Unreachable — caller path returns BeadsError before this.
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "doctor: db op reached execute_atomic; should be intercepted",
            ));
        }
    }
    Ok(())
}

/// Record a legacy-fixer disk mutation through the chokepoint without
/// rewriting the fixer to use [`Op::WriteFile`] / [`Op::DbExec`] directly.
///
/// Some `repair_*` paths (notably `VACUUM`, `REINDEX`, and the blocked-
/// cache rebuild through `reset_blocked_cache_table`) call into
/// fsqlite/config helpers that perform their own in-place file or DB
/// mutations and cannot be reformulated as a planned [`Op`] without a
/// larger refactor. Until that lands, this helper gives those callers
/// the same observability + recoverability the chokepoint provides:
///
/// 1. Read each target's bytes BEFORE the legacy work runs and compute
///    `before_hash`. Missing files use the empty-input sentinel.
/// 2. Enforce write_scopes for every target.
/// 3. Write a verbatim backup of each existing target to
///    `<run-dir>/backups/<rel-path>` and verify byte-identical via
///    `cmp_strict` (same belt-and-braces tripwire `mutate()` uses).
/// 4. Run the closure (the legacy fixer's actual work).
/// 5. Read each target AGAIN and compute `after_hash`.
/// 6. Append one `actions.jsonl` line per target with `op = "legacy_op"`,
///    `fixer_id` = the supplied identifier, and any closure error so
///    `br doctor undo` can replay the verbatim backup via its existing
///    `WriteFile`-from-backup recovery (the default branch of
///    [`super::surface::restore_one`] handles any non-rename, non-db
///    op by restoring the verbatim backup).
///
/// In `dry_run`, no backup is written, no `actions.jsonl` line is
/// appended, and the legacy closure is not invoked.
///
/// # Errors
///
/// - Returns [`BeadsError`] if any path is outside `write_scopes`.
/// - Returns the legacy closure's error unchanged if it faults.
/// - Returns [`BeadsError::Io`] if backup/hashing/log-write I/O fails.
///
/// The closure returns `()` because legacy fixer state is accumulated by
/// capturing mutable state from the caller.
pub fn record_legacy_op<F>(
    ctx: &MutateContext,
    fixer_id: &str,
    paths: &[&Path],
    legacy: F,
) -> Result<(), BeadsError>
where
    F: FnOnce() -> Result<(), BeadsError>,
{
    // (1) + (2) + (3): pre-state capture for every target. We collect
    // before doing any disk write so a precondition failure on path N
    // does not leave a partial backup tree behind for paths 0..N-1.
    let mut pre_state: Vec<(PathBuf, PathBuf, String, bool)> = Vec::with_capacity(paths.len());
    for path in paths {
        ensure_in_scope(&ctx.capabilities, path)?;
        let bytes_or_missing = match fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(BeadsError::Io(e)),
        };
        let before_hash = match &bytes_or_missing {
            Some(bytes) => sha256_hex_prefixed(bytes),
            None => SHA256_EMPTY_PREFIXED.to_string(),
        };
        let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
        let backup = ctx.run_dir.join("backups").join(rel);
        let existed = bytes_or_missing.is_some();
        if !ctx.dry_run
            && let Some(bytes) = bytes_or_missing
        {
            write_verbatim_backup(path, &backup, &bytes).map_err(BeadsError::Io)?;
            cmp_strict(path, &backup).map_err(BeadsError::Io)?;
        }
        pre_state.push((path.to_path_buf(), backup, before_hash, existed));
    }

    let started_at_ns = now_ns().saturating_sub(ctx.start_ns);

    if ctx.dry_run {
        for (path, _, _, _) in &pre_state {
            eprintln!("[dry-run] would mutate {}: legacy_op", path.display());
        }
        return Ok(());
    }

    // (4) Run the legacy work. If it errors, we still record the audit
    //     lines so the verbatim backups are linked to a JSONL entry the
    //     operator can find; otherwise the backup files would be
    //     orphaned under the run-dir.
    let outcome = legacy();
    let legacy_ok = outcome.is_ok();
    let legacy_error = outcome.as_ref().err().map(ToString::to_string);
    let finished_at_ns = now_ns().saturating_sub(ctx.start_ns);

    // (5) + (6): post-state hash + actions.jsonl line per path.
    for (path, _, before_hash, existed_before) in &pre_state {
        let rel = path.strip_prefix(&ctx.repo_root).unwrap_or(path);
        let after_bytes_or_missing = match fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(BeadsError::Io(e)),
        };
        let after_hash = match &after_bytes_or_missing {
            Some(bytes) => sha256_hex_prefixed(bytes),
            None => SHA256_EMPTY_PREFIXED.to_string(),
        };
        let record = ActionRecord {
            path: rel.to_string_lossy().into_owned(),
            op: "legacy_op",
            before_hash: before_hash.clone(),
            after_hash,
            started_at_ns,
            finished_at_ns,
            run_id: ctx.run_id.clone(),
            fixer_id: fixer_id.to_string(),
            ok: legacy_ok,
            rename_to: None,
            rolled_back: None,
            error: legacy_error.clone(),
        };
        let mut line = serde_json::to_string(&record).map_err(BeadsError::Json)?;
        // Tag whether the target file existed pre-mutation so `br
        // doctor undo` (when extended) can tell the difference between
        // "restore from verbatim backup" and "remove the post-creation
        // file" — the legacy ops currently only mutate existing files,
        // but recording the bit costs nothing and futureproofs the
        // audit log. We append the extra field manually because
        // ActionRecord is a stable wire contract shared with non-legacy
        // ops.
        if !existed_before {
            line = line.trim_end_matches('}').to_string() + ",\"existed_before\":false}";
        }
        line.push('\n');
        let mut f = ctx.actions_file.lock().map_err(|e| {
            BeadsError::internal(format!("doctor: actions_file mutex poisoned: {e}"))
        })?;
        f.write_all(line.as_bytes()).map_err(BeadsError::Io)?;
        f.sync_data().map_err(BeadsError::Io)?;
    }

    outcome
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use fsqlite::Connection;
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Build a [`MutateContext`] rooted at `tmp` with a fresh
    /// `actions.jsonl` and `backups/` subdir, optionally in dry-run.
    fn make_ctx(tmp: &Path, dry_run: bool) -> (MutateContext, PathBuf) {
        let run_id = "test-run".to_string();
        let run_dir = tmp.join(".doctor/runs").join(&run_id);
        fs::create_dir_all(run_dir.join("backups")).unwrap();
        let actions_path = run_dir.join("actions.jsonl");
        let actions_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&actions_path)
            .unwrap();

        let ctx = MutateContext {
            run_id,
            run_dir: run_dir.clone(),
            capabilities: Capabilities::for_repo(tmp),
            actions_file: Mutex::new(actions_file),
            fixer_id: "test-fixer".to_string(),
            repo_root: tmp.to_path_buf(),
            dry_run,
            start_ns: now_ns(),
        };
        (ctx, actions_path)
    }

    #[test]
    fn write_file_creates_backup_and_records_action() {
        let tmp = tempfile::tempdir().unwrap();
        // Set up a `.beads/` dir with an existing tracked file so we
        // exercise the backup path.
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("foo.txt");
        fs::write(&target, b"original").unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        let result = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"updated".to_vec(),
                mode: Some(0o644),
            },
        )
        .expect("mutate should succeed");

        assert!(result.ok);
        assert!(result.before_hash.starts_with("sha256:"));
        assert!(result.after_hash.starts_with("sha256:"));
        assert_ne!(result.before_hash, result.after_hash);

        // File contents updated.
        assert_eq!(fs::read(&target).unwrap(), b"updated");

        // Backup contains the original bytes byte-for-byte.
        let backup = ctx.run_dir.join("backups/.beads/foo.txt");
        assert!(backup.exists(), "backup must exist: {}", backup.display());
        assert_eq!(fs::read(&backup).unwrap(), b"original");

        // actions.jsonl has exactly one line of valid JSON.
        let f = std::fs::File::open(&actions_path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(f)
            .lines()
            .collect::<std::io::Result<_>>()
            .unwrap();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["op"], "write_file");
        assert_eq!(v["fixer_id"], "test-fixer");
        assert_eq!(v["run_id"], "test-run");
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn dry_run_does_not_touch_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("dry.txt");
        fs::write(&target, b"keep me").unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), true);

        let result = mutate(
            &ctx,
            &target,
            Op::WriteFile {
                content: b"would not write".to_vec(),
                mode: Some(0o644),
            },
        )
        .expect("dry-run mutate should succeed");

        assert!(result.ok);
        // before_hash == after_hash in dry-run mode.
        assert_eq!(result.before_hash, result.after_hash);
        // File unchanged.
        assert_eq!(fs::read(&target).unwrap(), b"keep me");
        // No backup created.
        let backup = ctx.run_dir.join("backups/.beads/dry.txt");
        assert!(!backup.exists());
        // No actions.jsonl line written.
        assert_eq!(fs::metadata(&actions_path).unwrap().len(), 0);
    }

    #[test]
    fn legacy_op_dry_run_does_not_invoke_legacy_closure() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("legacy.txt");
        fs::write(&target, b"original").unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), true);
        let invoked = std::cell::Cell::new(false);

        record_legacy_op(&ctx, "legacy-dry-run", &[&target], || {
            invoked.set(true);
            fs::write(&target, b"mutated").map_err(BeadsError::Io)?;
            Ok(())
        })
        .expect("dry-run legacy op should succeed");

        assert!(
            !invoked.get(),
            "dry-run legacy op must not invoke the mutating closure"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"original",
            "dry-run legacy op must leave target bytes unchanged"
        );
        let backup = ctx.run_dir.join("backups/.beads/legacy.txt");
        assert!(
            !backup.exists(),
            "dry-run legacy op must not create a backup"
        );
        assert_eq!(
            fs::metadata(&actions_path).unwrap().len(),
            0,
            "dry-run legacy op must not append actions.jsonl"
        );
    }

    #[test]
    fn legacy_op_records_closure_error_in_actions_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("legacy-error.txt");
        fs::write(&target, b"original").unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        let err = record_legacy_op(&ctx, "legacy-error", &[&target], || {
            fs::write(&target, b"partially mutated").map_err(BeadsError::Io)?;
            Err(BeadsError::Config("legacy fixer failed".to_string()))
        })
        .expect_err("legacy closure error should be propagated");
        assert!(
            err.to_string().contains("legacy fixer failed"),
            "unexpected propagated error: {err}"
        );

        let backup = ctx.run_dir.join("backups/.beads/legacy-error.txt");
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"original",
            "legacy failure must still retain the pre-mutation backup"
        );

        let lines = fs::read_to_string(actions_path).unwrap();
        let action: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(action["op"], "legacy_op");
        assert_eq!(action["fixer_id"], "legacy-error");
        assert_eq!(action["ok"], false);
        assert!(
            action["error"]
                .as_str()
                .is_some_and(|msg| msg.contains("legacy fixer failed")),
            "legacy action should preserve closure error text: {action}"
        );
    }

    #[test]
    fn rename_moves_file_and_after_hash_is_empty_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let src = beads_dir.join("orphan.lock");
        fs::write(&src, b"lock-bytes").unwrap();

        let (ctx, _) = make_ctx(tmp.path(), false);
        let dst = ctx.run_dir.join("quarantine/orphan.lock");

        let result =
            mutate(&ctx, &src, Op::Rename { to: dst.clone() }).expect("rename should succeed");

        assert!(result.ok);
        // before_hash should be a real hash of "lock-bytes".
        assert_ne!(result.before_hash, SHA256_EMPTY_PREFIXED);
        // after_hash should be the empty-file sentinel because src is
        // gone.
        assert_eq!(result.after_hash, SHA256_EMPTY_PREFIXED);

        assert!(!src.exists(), "source must be moved");
        assert!(dst.exists(), "destination must exist");
        assert_eq!(fs::read(&dst).unwrap(), b"lock-bytes");
    }

    #[test]
    fn append_file_preserves_backed_up_bytes_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("append.log");
        fs::write(&target, b"before").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), false);
        let result = mutate(
            &ctx,
            &target,
            Op::AppendFile {
                content: b"-after".to_vec(),
            },
        )
        .expect("append should succeed");

        assert!(result.ok);
        assert_eq!(fs::read(&target).unwrap(), b"before-after");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read(ctx.run_dir.join("backups/.beads/append.log")).unwrap(),
            b"before"
        );

        let lines = fs::read_to_string(actions_path).unwrap();
        let action: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(action["op"], "append_file");
        assert_eq!(action["ok"], true);
    }

    #[test]
    fn out_of_scope_path_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let (ctx, _) = make_ctx(tmp.path(), false);

        // /tmp/foo is well outside `.beads/` and `.doctor/`.
        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("victim.txt");
        fs::write(&outside_target, b"untouched").unwrap();

        let err = mutate(
            &ctx,
            &outside_target,
            Op::WriteFile {
                content: b"oops".to_vec(),
                mode: None,
            },
        )
        .expect_err("out-of-scope writes must be refused");
        assert!(
            err.to_string().contains("outside write_scopes"),
            "error must mention scope refusal: {err}"
        );
        // File untouched.
        assert_eq!(fs::read(&outside_target).unwrap(), b"untouched");
    }

    #[test]
    fn rename_destination_outside_write_scope_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let source = beads_dir.join("keep-inside.txt");
        fs::write(&source, b"keep inside").unwrap();

        let (ctx, actions_path) = make_ctx(tmp.path(), false);
        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("escaped.txt");

        let err = mutate(
            &ctx,
            &source,
            Op::Rename {
                to: outside_target.clone(),
            },
        )
        .expect_err("out-of-scope rename destinations must be refused");
        assert!(
            err.to_string().contains("outside write_scopes"),
            "error must mention scope refusal: {err}"
        );
        assert_eq!(fs::read(&source).unwrap(), b"keep inside");
        assert!(!outside_target.exists());
        assert_eq!(fs::metadata(&actions_path).unwrap().len(), 0);
        let backup = ctx.run_dir.join("backups/.beads/keep-inside.txt");
        assert!(!backup.exists());
    }

    /// Regression for the round-3 fresh-eyes TOCTOU fix:
    /// `execute_atomic(Op::AppendFile)` must merge against the bytes
    /// supplied by the chokepoint, not against a fresh read of the live
    /// file. The caller has already hashed and backed up those supplied
    /// bytes; a later live read could include data that undo cannot
    /// restore.
    #[test]
    fn append_file_anchors_to_supplied_bytes_not_live_file() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("append.log");
        fs::write(&target, b"hijacked\n").unwrap();

        execute_atomic(
            &target,
            Op::AppendFile {
                content: b"new\n".to_vec(),
            },
            Some(b"original\n"),
            Some(0o600),
        )
        .expect("append should use supplied bytes");

        assert_eq!(fs::read(&target).unwrap(), b"original\nnew\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // ========================================================================
    // WP4 — DB ops (DbExec / DbMigrate)
    // ========================================================================

    /// Set up a fresh `.beads/beads.db` with a single sample table for
    /// the DB-op tests. Returns the absolute path to the DB.
    fn setup_test_db(tmp: &Path) -> PathBuf {
        let beads_dir = tmp.join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db = beads_dir.join("beads.db");
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sample_widgets (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                value INTEGER NOT NULL DEFAULT 0
            )",
        )
        .unwrap();
        let _ = conn.close();
        db
    }

    #[test]
    fn db_snapshot_writer_never_overwrites_same_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let backups_db = tmp.path();
        let first = write_unique_db_snapshot_with_stamp(
            backups_db,
            "sample_widgets",
            "abcdef12",
            42,
            b"first",
        )
        .expect("first snapshot");
        let second = write_unique_db_snapshot_with_stamp(
            backups_db,
            "sample_widgets",
            "abcdef12",
            42,
            b"second",
        )
        .expect("second snapshot");

        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        assert!(
            second
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("__001.json")),
            "collision suffix should be visible in second filename: {}",
            second.display()
        );
    }

    #[test]
    fn test_db_exec_snapshots_affected_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = setup_test_db(tmp.path());
        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        // Pre-state: empty table → snapshot must reflect that.
        let before_sha_path = sha256_file_hex_prefixed(&db).unwrap();
        assert!(before_sha_path.starts_with("sha256:"));

        let result = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO sample_widgets(name, value) VALUES (?, ?)".into(),
                args: vec![DbArg::Text("alpha".into()), DbArg::I64(7)],
                affected_tables: vec!["sample_widgets".into()],
                affected_predicate: None,
            },
        )
        .expect("DbExec should succeed");
        assert!(result.ok);
        assert_ne!(
            result.before_hash, result.after_hash,
            "after_hash must differ from before_hash because the DB grew"
        );

        // Snapshot file exists, captures empty pre-state.
        let snapshot_dir = ctx.run_dir.join("backups/db");
        let entries: Vec<_> = fs::read_dir(&snapshot_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one snapshot file");
        let body = fs::read_to_string(entries[0].path()).unwrap();
        let snap: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(snap["table"], "sample_widgets");
        assert_eq!(snap["rows"].as_array().unwrap().len(), 0);
        assert_eq!(
            snap["columns"].as_array().unwrap(),
            &vec![
                serde_json::Value::String("id".into()),
                serde_json::Value::String("name".into()),
                serde_json::Value::String("value".into()),
            ]
        );

        // Post-state: row is in the DB.
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        let rows = conn
            .query("SELECT name, value FROM sample_widgets")
            .unwrap();
        assert_eq!(rows.len(), 1);
        let _ = conn.close();

        // actions.jsonl carries one DbExec line.
        let log = fs::read_to_string(&actions_path).unwrap();
        let line = log.lines().next().expect("at least one action line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["op"], "db_exec");
        assert_eq!(v["affected_tables"], "sample_widgets");
        let hashes = v["db_snapshot_sha256"].as_array().unwrap();
        assert_eq!(hashes.len(), 1);
        assert_eq!(
            hashes[0].as_str().unwrap(),
            sha256_hex_prefixed(body.as_bytes())
        );
    }

    #[test]
    fn test_db_exec_rollback_on_constraint_violation() {
        let tmp = tempfile::tempdir().unwrap();
        let db = setup_test_db(tmp.path());

        // Seed an existing row so a duplicate INSERT trips UNIQUE.
        {
            let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            conn.execute("INSERT INTO sample_widgets(name, value) VALUES ('dup', 1)")
                .unwrap();
            let _ = conn.close();
        }

        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        let err = mutate(
            &ctx,
            &db,
            Op::DbExec {
                sql: "INSERT INTO sample_widgets(name, value) VALUES (?, ?)".into(),
                args: vec![DbArg::Text("dup".into()), DbArg::I64(2)],
                affected_tables: vec!["sample_widgets".into()],
                affected_predicate: None,
            },
        )
        .expect_err("UNIQUE violation should propagate");

        // Error mentions a database / constraint failure.
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unique") || msg.to_lowercase().contains("constraint"),
            "expected unique/constraint failure; got {msg}"
        );

        // No actions.jsonl line was written.
        let log_len = fs::metadata(&actions_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(log_len, 0, "actions.jsonl must remain empty on rollback");

        // Behavioral check: the rolled-back transaction left the table
        // exactly one row (the seed). We do not compare DB file bytes
        // because fsqlite touches header counters even on a rolled-back
        // transaction; the row-level invariant is what matters.
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        let rows = conn.query("SELECT COUNT(*) FROM sample_widgets").unwrap();
        assert_eq!(
            rows[0].get(0).and_then(|v| match v {
                fsqlite_types::value::SqliteValue::Integer(n) => Some(*n),
                _ => None,
            }),
            Some(1),
            "rollback must leave only the seed row in the table"
        );
        let _ = conn.close();

        // Orphan snapshot scrub: the chokepoint snapshotted the table
        // before the SQL fired, but the SQL rolled back. There must be
        // NO snapshot files left under backups/db/ — otherwise the
        // workspace carries a dangling artifact with no actions.jsonl
        // entry referencing it.
        let snapshot_dir = ctx.run_dir.join("backups/db");
        if snapshot_dir.exists() {
            let leftover: Vec<_> = fs::read_dir(&snapshot_dir)
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .collect();
            assert!(
                leftover.is_empty(),
                "rollback must scrub orphan snapshots; found {leftover:?}",
            );
        }
    }

    #[test]
    fn test_db_migrate_user_version_mismatch_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let db = setup_test_db(tmp.path());

        // Stamp user_version = 5 directly.
        {
            let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            conn.execute("PRAGMA user_version = 5").unwrap();
            let _ = conn.close();
        }

        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        let err = mutate(&ctx, &db, Op::DbMigrate { from: 4, to: 6 })
            .expect_err("user_version mismatch must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("user_version mismatch"),
            "error should reference user_version mismatch; got {msg}"
        );

        // PRAGMA user_version unchanged: this is the behavioral
        // invariant the precondition gate guarantees.
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        let row = conn.query_row("PRAGMA user_version").unwrap();
        let v = match row.get(0) {
            Some(fsqlite_types::value::SqliteValue::Integer(n)) => *n,
            _ => -1,
        };
        assert_eq!(v, 5, "user_version must not change after refusal");
        let _ = conn.close();

        // No actions.jsonl line was written.
        let log_len = fs::metadata(&actions_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(log_len, 0);

        // The orphan pre-migrate snapshot was scrubbed when we refused.
        let snap = ctx.run_dir.join("backups/db/beads.db.pre-migrate");
        assert!(
            !snap.exists(),
            "refused migrate must not leave a stale snapshot at {}",
            snap.display()
        );
    }

    #[test]
    fn test_db_migrate_happy_path_writes_backup_and_runs_ddl() {
        let tmp = tempfile::tempdir().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db = beads_dir.join("beads.db");

        // Stand up a real beads DB via apply_schema, then stamp
        // user_version back to a pre-current value so the chokepoint
        // migration has a real upgrade to run.
        {
            let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            crate::storage::schema::apply_schema(&conn).expect("apply_schema on fresh test DB");
            // Demote user_version so run_migrations_atomic has work to do.
            conn.execute("PRAGMA user_version = 7").unwrap();
            let _ = conn.close();
        }

        let pre_size = fs::metadata(&db).unwrap().len();
        let (ctx, actions_path) = make_ctx(tmp.path(), false);

        let result = mutate(
            &ctx,
            &db,
            Op::DbMigrate {
                from: 7,
                to: crate::storage::schema::CURRENT_SCHEMA_VERSION as u32,
            },
        )
        .expect("DbMigrate should run schema migrations end-to-end");
        assert!(result.ok);

        // The pre-migrate snapshot is preserved verbatim for undo.
        let snap = ctx.run_dir.join("backups/db/beads.db.pre-migrate");
        assert!(
            snap.exists(),
            "pre-migrate snapshot missing: {}",
            snap.display()
        );
        let snap_size = fs::metadata(&snap).unwrap().len();
        assert_eq!(
            snap_size, pre_size,
            "snapshot length must match pre-migrate DB length"
        );
        let snap_bytes = fs::read(&snap).unwrap();
        assert!(
            snap_bytes.starts_with(b"SQLite format 3\0"),
            "snapshot must be a valid SQLite file (header check)"
        );

        // The DDL actually ran: PRAGMA user_version is now CURRENT_SCHEMA_VERSION.
        let target = crate::storage::schema::CURRENT_SCHEMA_VERSION as u32;
        {
            let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            let row = conn.query_row("PRAGMA user_version").unwrap();
            let v = match row.get(0) {
                Some(fsqlite_types::value::SqliteValue::Integer(n)) => u32::try_from(*n).unwrap(),
                _ => 0,
            };
            assert_eq!(
                v, target,
                "post-migrate user_version should be CURRENT_SCHEMA_VERSION ({target})"
            );
            // v9 unconditionally adds close_metadata.
            let rows = conn
                .query(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name='close_metadata'",
                )
                .unwrap();
            assert_eq!(
                rows.len(),
                1,
                "v9 migration must have created close_metadata table"
            );
            let _ = conn.close();
        }

        // actions.jsonl records the op and the version transition; the
        // `migration_logic_not_yet_routed` warning is GONE now that
        // beads_rust-folg has wired schema.rs through the chokepoint.
        let log = fs::read_to_string(&actions_path).unwrap();
        let line = log.lines().next().expect("expected one action line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["op"], "db_migrate");
        assert_eq!(v["migrate_from"], 7);
        assert_eq!(v["migrate_to"], i64::from(target));
        assert!(
            v.get("warning").is_none() || v["warning"].is_null(),
            "warning field should be absent now that DDL routing is wired: {v}"
        );
    }

    /// Property-based reversibility fuzzing (hardens the `reversibility`
    /// and `test_coverage_of_repair` rubric dimensions).
    ///
    /// The chokepoint's whole reversibility guarantee rests on ONE
    /// invariant: every `mutate()` captures a byte-faithful verbatim
    /// backup of the pre-mutation file, and records a `before_hash` that
    /// is exactly the SHA-256 of those bytes. If that holds for ARBITRARY
    /// content, then `doctor undo` (which is restore-from-backup for the
    /// file ops) is correct by construction.
    ///
    /// These properties fuzz the invariant across random byte payloads —
    /// empty files, binary content, embedded NULs and newlines, and
    /// content that does or doesn't already coincide with the new bytes —
    /// the exact edge cases hand-written fixtures tend to miss. Each case
    /// drives the real `mutate()` forward and then performs the logical
    /// undo (rewrite the backup bytes to the path), asserting
    /// byte-identity with the original.
    mod reversibility_props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// `Op::WriteFile` over arbitrary original/updated byte vectors:
            /// the backup must equal the original, `before_hash` must be the
            /// original's SHA-256, and the logical undo must restore the
            /// original byte-for-byte.
            #[test]
            fn prop_writefile_backup_roundtrips_any_bytes(
                original in proptest::collection::vec(any::<u8>(), 0..2048),
                updated in proptest::collection::vec(any::<u8>(), 0..2048),
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("rt.bin");
                fs::write(&target, &original).unwrap();

                let (ctx, _ap) = make_ctx(tmp.path(), false);
                let result = mutate(
                    &ctx,
                    &target,
                    Op::WriteFile { content: updated.clone(), mode: None },
                ).unwrap();

                // Forward mutation applied.
                prop_assert_eq!(fs::read(&target).unwrap(), updated.clone());
                // before_hash is the SHA-256 of the ORIGINAL bytes.
                prop_assert_eq!(&result.before_hash, &sha256_hex_prefixed(&original));
                // Verbatim backup byte-equals the original.
                let backup = ctx.run_dir.join("backups/.beads/rt.bin");
                prop_assert_eq!(fs::read(&backup).unwrap(), original.clone());
                // Logical undo (restore backup bytes) → byte-identical original.
                let restored = fs::read(&backup).unwrap();
                fs::write(&target, &restored).unwrap();
                prop_assert_eq!(fs::read(&target).unwrap(), original);
            }

            /// `Op::AppendFile` over arbitrary original/suffix byte vectors:
            /// the live file becomes original++suffix, the backup preserves
            /// the pre-append original, and the logical undo restores it.
            #[test]
            fn prop_appendfile_backup_roundtrips_any_bytes(
                original in proptest::collection::vec(any::<u8>(), 1..2048),
                suffix in proptest::collection::vec(any::<u8>(), 0..2048),
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("append-rt.bin");
                fs::write(&target, &original).unwrap();

                let (ctx, _ap) = make_ctx(tmp.path(), false);
                mutate(
                    &ctx,
                    &target,
                    Op::AppendFile { content: suffix.clone() },
                ).unwrap();

                // Forward: target is original ++ suffix.
                let mut expected = original.clone();
                expected.extend_from_slice(&suffix);
                prop_assert_eq!(fs::read(&target).unwrap(), expected);
                // Backup byte-equals the pre-append original.
                let backup = ctx.run_dir.join("backups/.beads/append-rt.bin");
                prop_assert_eq!(fs::read(&backup).unwrap(), original.clone());
                // Logical undo restores the original.
                let restored = fs::read(&backup).unwrap();
                fs::write(&target, &restored).unwrap();
                prop_assert_eq!(fs::read(&target).unwrap(), original);
            }

            /// `Op::Chmod` reversibility: the chokepoint stamps the
            /// verbatim backup with the source's ORIGINAL mode, so undo
            /// can restore it. Modes are drawn from `0o400..=0o777` so the
            /// owner-read bit is always set (the chokepoint must be able to
            /// read the file to back it up, and the test must re-read it to
            /// assert). Content is left untouched by a chmod, so the backup
            /// also byte-preserves it.
            #[test]
            fn prop_chmod_backup_preserves_original_mode(
                content in proptest::collection::vec(any::<u8>(), 0..512),
                m1 in 0o400u32..=0o777,
                m2 in 0o400u32..=0o777,
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("chmod-rt.bin");
                fs::write(&target, &content).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(m1)).unwrap();

                let (ctx, _ap) = make_ctx(tmp.path(), false);
                mutate(&ctx, &target, Op::Chmod { mode: m2 }).unwrap();

                // Content is unchanged by a chmod.
                prop_assert_eq!(fs::read(&target).unwrap(), content.clone());
                // Live mode is now m2.
                prop_assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    m2 & 0o777
                );
                // Backup preserved BOTH the original bytes and the original mode.
                let backup = ctx.run_dir.join("backups/.beads/chmod-rt.bin");
                prop_assert_eq!(fs::read(&backup).unwrap(), content);
                prop_assert_eq!(
                    fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                    m1 & 0o777
                );
                // Logical undo (restore the backup's mode) → mode back to m1.
                let backup_mode = fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
                fs::set_permissions(&target, fs::Permissions::from_mode(backup_mode)).unwrap();
                prop_assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    m1 & 0o777
                );
            }
        }
    }

    /// Metamorphic idempotence properties (hardens the idempotence
    /// dimension at the op level).
    ///
    /// The fixture `REPLAY_IDEMPOTENCE=1` gate proves the FIXER level
    /// ("a second `--repair` is a no-op") end-to-end. These properties
    /// pin the OP-level basis underneath it, and — importantly — encode
    /// the asymmetry that explains WHY append-based fixers must
    /// detect-guard while write/chmod-based fixers need not:
    ///
    /// - `WriteFile` and `Chmod` CONVERGE: re-applying the same op to its
    ///   own output is a content/mode no-op.
    /// - `AppendFile` does NOT converge: the chokepoint never dedupes, so
    ///   appending the same suffix twice duplicates it. Fixers using
    ///   AppendFile (trailing-newline, inner-gitignore) therefore MUST
    ///   only fire when the detector says the content is missing.
    mod idempotence_props {
        use super::*;
        use proptest::prelude::*;

        /// Build a MutateContext for repo `tmp` with a distinct `run` id.
        /// Each `br doctor --repair` invocation gets its own run-dir, so
        /// modelling "repair twice" requires two distinct run-dirs — a
        /// single shared ctx would collide on the backup path (and a
        /// chmod-to-readonly would even make the shared backup
        /// un-overwritable on the second call).
        fn ctx_with_run(tmp: &Path, run: &str) -> MutateContext {
            let run_dir = tmp.join(".doctor/runs").join(run);
            fs::create_dir_all(run_dir.join("backups")).unwrap();
            let actions_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(run_dir.join("actions.jsonl"))
                .unwrap();
            MutateContext {
                run_id: run.to_string(),
                run_dir,
                capabilities: Capabilities::for_repo(tmp),
                actions_file: Mutex::new(actions_file),
                fixer_id: "test-fixer".to_string(),
                repo_root: tmp.to_path_buf(),
                dry_run: false,
                start_ns: now_ns(),
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Re-writing the same content is a content no-op: the second
            /// run's before_hash == after_hash and the file is unchanged.
            #[test]
            fn prop_writefile_second_apply_is_content_noop(
                original in proptest::collection::vec(any::<u8>(), 0..1024),
                target_content in proptest::collection::vec(any::<u8>(), 0..1024),
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("idem-write.bin");
                fs::write(&target, &original).unwrap();

                let ctx1 = ctx_with_run(tmp.path(), "run-1");
                mutate(&ctx1, &target, Op::WriteFile { content: target_content.clone(), mode: None }).unwrap();
                let ctx2 = ctx_with_run(tmp.path(), "run-2");
                let r2 = mutate(&ctx2, &target, Op::WriteFile { content: target_content.clone(), mode: None }).unwrap();

                // Second application converged: no content change.
                prop_assert_eq!(&r2.before_hash, &r2.after_hash);
                prop_assert_eq!(fs::read(&target).unwrap(), target_content);
            }

            /// Re-applying the same mode is a no-op.
            #[test]
            fn prop_chmod_second_apply_is_mode_noop(
                content in proptest::collection::vec(any::<u8>(), 0..256),
                m in 0o400u32..=0o777,
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("idem-chmod.bin");
                fs::write(&target, &content).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(m)).unwrap();

                let ctx1 = ctx_with_run(tmp.path(), "run-1");
                mutate(&ctx1, &target, Op::Chmod { mode: m }).unwrap();
                let ctx2 = ctx_with_run(tmp.path(), "run-2");
                mutate(&ctx2, &target, Op::Chmod { mode: m }).unwrap();

                prop_assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    m & 0o777
                );
            }

            /// AppendFile does NOT converge: two appends of the same
            /// non-empty suffix duplicate it. Encodes the design invariant
            /// that append fixers must detect-guard.
            #[test]
            fn prop_appendfile_is_not_idempotent(
                original in proptest::collection::vec(any::<u8>(), 0..512),
                suffix in proptest::collection::vec(any::<u8>(), 1..512),
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let beads_dir = tmp.path().join(".beads");
                fs::create_dir_all(&beads_dir).unwrap();
                let target = beads_dir.join("idem-append.bin");
                fs::write(&target, &original).unwrap();

                let ctx1 = ctx_with_run(tmp.path(), "run-1");
                mutate(&ctx1, &target, Op::AppendFile { content: suffix.clone() }).unwrap();
                let ctx2 = ctx_with_run(tmp.path(), "run-2");
                mutate(&ctx2, &target, Op::AppendFile { content: suffix.clone() }).unwrap();

                // original ++ suffix ++ suffix — the append was NOT deduped.
                let mut expected = original.clone();
                expected.extend_from_slice(&suffix);
                expected.extend_from_slice(&suffix);
                prop_assert_eq!(fs::read(&target).unwrap(), expected);
            }
        }
    }
}
