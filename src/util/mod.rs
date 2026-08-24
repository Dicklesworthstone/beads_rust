//! Shared utilities for `beads_rust`.
//!
//! Common functionality used across modules:
//! - Content hashing (SHA256)
//! - Time parsing and formatting (RFC3339)
//! - Path handling (.beads discovery)
//! - ID generation (base36 adaptive)
//! - Last-touched tracking
//! - Progress indicators (for long-running operations)

mod hash;
pub mod id;
pub mod markdown_import;
pub mod progress;
pub mod time;

pub use hash::{ContentHashable, content_hash, content_hash_from_parts, hex_encode};
pub use id::{
    IdConfig, IdGenerator, IdResolver, MatchType, ParsedId, ResolvedId, ResolverConfig, child_id,
    find_matching_ids, id_depth, is_child_id, is_valid_id_format, normalize_id, parse_id,
    resolve_id, validate_prefix,
};

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const LAST_TOUCHED_FILE: &str = "last-touched";
const MAX_LAST_TOUCHED_BYTES: usize = 4096;
const MAX_LAST_TOUCHED_BYTES_U64: u64 = 4096;

/// Environment variable for overriding the cache directory location.
///
/// When set, transient files like `last-touched` will be stored in this
/// directory instead of the `.beads` directory. This is useful for monorepo
/// setups where the `.beads` directory is checked into version control but
/// transient cache files should be stored elsewhere.
pub const BEADS_CACHE_DIR_ENV: &str = "BEADS_CACHE_DIR";

/// Resolve the effective cache directory for transient files.
///
/// Priority:
/// 1. `BEADS_CACHE_DIR` environment variable (if set and valid)
/// 2. The beads_dir itself (default behavior)
#[must_use]
pub fn resolve_cache_dir(beads_dir: &Path) -> PathBuf {
    if let Ok(cache_dir) = env::var(BEADS_CACHE_DIR_ENV) {
        let path = PathBuf::from(&cache_dir);
        if !cache_dir.is_empty() {
            return path;
        }
    }
    beads_dir.to_path_buf()
}

/// Build the path to the `last-touched` file.
///
/// The file location is determined by:
/// 1. `BEADS_CACHE_DIR` environment variable (if set)
/// 2. The `.beads` directory (default)
#[must_use]
pub fn last_touched_path(beads_dir: &Path) -> PathBuf {
    resolve_cache_dir(beads_dir).join(LAST_TOUCHED_FILE)
}

const DB_FILE: &str = "beads.db";

/// Build the path to the SQLite database file.
///
/// The file location is determined by:
/// 1. `BEADS_CACHE_DIR` environment variable (if set)
/// 2. The `.beads` directory (default)
///
/// This allows storing the database (and its WAL/SHM files) on a fast local
/// filesystem when the `.beads` directory is on a slow network mount.
#[must_use]
pub fn db_path(beads_dir: &Path) -> PathBuf {
    resolve_cache_dir(beads_dir).join(DB_FILE)
}

/// Best-effort write of the last-touched issue ID.
///
/// Errors are ignored to match classic bd behavior.
/// If `BEADS_CACHE_DIR` is set, the cache directory will be created if needed.
pub fn set_last_touched_id(beads_dir: &Path, id: &str) {
    let path = last_touched_path(beads_dir);

    // Ensure cache directory exists (best-effort)
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    if let Ok(mut file) = options.open(path) {
        let _ = writeln!(file, "{id}");
    }
}

/// Read the last-touched issue ID.
///
/// Returns an empty string if the file is missing or unreadable.
#[must_use]
pub fn get_last_touched_id(beads_dir: &Path) -> String {
    let path = last_touched_path(beads_dir);
    let Ok(metadata) = fs::metadata(&path) else {
        return String::new();
    };

    read_last_touched_file_limited(&path, &metadata).unwrap_or_default()
}

fn read_last_touched_file_limited(path: &Path, metadata: &fs::Metadata) -> io::Result<String> {
    if metadata.len() > MAX_LAST_TOUCHED_BYTES_U64 {
        return Ok(String::new());
    }

    let file = fs::File::open(path)?;
    let mut reader = file.take(MAX_LAST_TOUCHED_BYTES_U64.saturating_add(1));
    let mut content = Vec::new();
    reader.read_to_end(&mut content)?;
    if content.len() > MAX_LAST_TOUCHED_BYTES {
        return Ok(String::new());
    }

    let content =
        String::from_utf8(content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(content
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// Best-effort delete of the last-touched file.
pub fn clear_last_touched(beads_dir: &Path) {
    let path = last_touched_path(beads_dir);
    let _ = fs::remove_file(path);
}

/// Rename a staged file into place and fsync the affected parent directories.
///
/// Atomic `rename` protects readers from partial files, but on Unix the rename
/// itself is not guaranteed durable across power loss until the containing
/// directory is synced. On non-Unix targets Rust does not expose a portable
/// directory fsync API, so this preserves the existing atomic rename behavior
/// and returns success after the rename.
pub fn durable_rename(from: &Path, to: &Path) -> io::Result<()> {
    durable_rename_with_parent_sync(from, to, sync_directory)
}

/// Fsync both parent directories after a rename whose namespace mutation has
/// already succeeded.
///
/// Callers that maintain rollback or inode-authority bookkeeping should use a
/// plain `fs::rename`, record the successful namespace change, and then call
/// this function. That ordering keeps a post-rename fsync error distinguishable
/// from a rename that never happened.
pub(crate) fn sync_rename_parent_directories(from: &Path, to: &Path) -> io::Result<()> {
    sync_rename_parent_directories_with(from, to, sync_directory)
}

fn durable_rename_with_parent_sync<F>(from: &Path, to: &Path, sync_dir: F) -> io::Result<()>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    fs::rename(from, to)?;
    sync_rename_parent_directories_with(from, to, sync_dir)
}

fn sync_rename_parent_directories_with<F>(from: &Path, to: &Path, mut sync_dir: F) -> io::Result<()>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    let target_parent = parent_for_directory_sync(to);
    sync_dir(target_parent)?;

    let source_parent = parent_for_directory_sync(from);
    if source_parent != target_parent {
        sync_dir(source_parent)?;
    }

    Ok(())
}

/// Fsync the parent directory for a newly created or replaced path.
///
/// This is needed after creating files with `create_new` as well as after
/// durable renames so the directory entry is durable, not just the file data.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    sync_directory(parent_for_directory_sync(path))
}

fn parent_for_directory_sync(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> io::Result<()> {
    tracing::debug!(
        path = %path.display(),
        "Skipping parent directory fsync: no portable directory fsync on this target"
    );
    Ok(())
}

#[cfg(test)]
pub mod test_helpers {
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};
    use tempfile::TempDir;
    pub static TEST_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Whether `start` or any ancestor already contains a beads workspace or a
    /// git checkout. Both matter: `br` resolves its workspace by walking
    /// ancestors for `.beads/`, and the sync status helpers shell out to git,
    /// so a temp directory inside either tree is not the isolated environment
    /// these tests intend.
    fn is_inside_beads_workspace(start: &Path) -> bool {
        start.ancestors().any(|dir| {
            dir.join(".beads").is_dir() || dir.join("_beads").is_dir() || dir.join(".git").exists()
        })
    }

    /// A temp root guaranteed not to sit inside an existing beads workspace.
    ///
    /// [`crate::config::discover_beads_dir`] walks every ancestor to the
    /// filesystem root with no `.git`-style boundary, so a `TMPDIR` inside a
    /// checkout hands each "fresh" temp directory the enclosing repository's
    /// `.beads/`. Tests that assert on an *uninitialized* workspace then
    /// silently exercise the wrong one and fail.
    ///
    /// This is not hypothetical: rch points `TMPDIR` at `<repo>/.rch-tmp` for
    /// remote builds, so these tests fail on every worker while passing
    /// locally. `tests/common/cli.rs` carries the same helper for the
    /// integration suite, which cannot be shared with `src/` unit tests.
    ///
    /// Prefer `TMPDIR`; fall back to a system temp root that is clean.
    ///
    /// # Panics
    ///
    /// Panics when `TMPDIR` and every system fallback sit inside a beads
    /// workspace or git checkout, since silently returning a polluted root
    /// would make the calling test assert against the wrong workspace.
    #[must_use]
    pub fn isolated_temp_root() -> PathBuf {
        let preferred = std::env::temp_dir();
        if !is_inside_beads_workspace(&preferred) {
            return preferred;
        }

        for fallback in ["/tmp", "/var/tmp"] {
            let path = PathBuf::from(fallback);
            if path.is_dir() && !is_inside_beads_workspace(&path) {
                return path;
            }
        }

        panic!(
            "no beads-free temp root available: TMPDIR ({}) is inside a beads workspace \
             and no system fallback is clean. Set TMPDIR to a directory outside any \
             .beads/ tree before running the test suite.",
            preferred.display()
        );
    }

    /// A [`TempDir`] created outside any beads workspace.
    ///
    /// Use this instead of `TempDir::new()` in any test whose subject resolves
    /// a beads workspace by walking ancestors.
    ///
    /// # Panics
    ///
    /// Panics when no clean temp root exists or the directory cannot be
    /// created.
    #[must_use]
    pub fn isolated_temp_dir() -> TempDir {
        TempDir::new_in(isolated_temp_root()).expect("isolated temp dir")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn isolated_temp_root_is_beads_free_and_usable() {
        let root = test_helpers::isolated_temp_root();
        assert!(
            root.is_dir(),
            "temp root {} is not a directory",
            root.display()
        );
        // The whole point: nothing above a temp dir created here may supply a
        // workspace to `discover_beads_dir`'s unbounded ancestor walk.
        let temp = test_helpers::isolated_temp_dir();
        assert!(
            crate::config::discover_beads_dir(Some(temp.path())).is_err(),
            "temp dir {} resolved a beads workspace; TMPDIR is inside one",
            temp.path().display()
        );
    }

    #[test]
    fn test_set_get_clear_last_touched() {
        let temp = TempDir::new().expect("temp dir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir(&beads_dir).expect("create .beads");

        assert_eq!(get_last_touched_id(&beads_dir), "");

        set_last_touched_id(&beads_dir, "bd-abc123");
        assert_eq!(get_last_touched_id(&beads_dir), "bd-abc123");

        clear_last_touched(&beads_dir);
        assert_eq!(get_last_touched_id(&beads_dir), "");
    }

    #[cfg(unix)]
    #[test]
    fn test_last_touched_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir(&beads_dir).expect("create .beads");

        set_last_touched_id(&beads_dir, "bd-abc123");
        let metadata = fs::metadata(last_touched_path(&beads_dir)).expect("metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn test_set_last_touched_creates_parent_dir() {
        // Test that set_last_touched_id creates the parent directory if needed
        let temp = TempDir::new().expect("temp dir");
        let cache_dir = temp.path().join("nested").join("cache");
        // cache_dir doesn't exist yet

        // Create last-touched path manually (simulating what happens with BEADS_CACHE_DIR)
        let path = cache_dir.join(LAST_TOUCHED_FILE);

        // Manually test the parent directory creation logic
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        assert!(cache_dir.exists(), "parent dir should be created");
    }

    #[test]
    fn test_get_last_touched_ignores_oversized_cache_file() {
        let temp = TempDir::new().expect("temp dir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir(&beads_dir).expect("create .beads");
        let path = last_touched_path(&beads_dir);
        fs::write(&path, "bd-abc123\n").expect("write last touched");
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open last touched");
        file.set_len(MAX_LAST_TOUCHED_BYTES_U64 + 1)
            .expect("extend last touched");

        assert_eq!(get_last_touched_id(&beads_dir), "");
    }

    #[test]
    fn test_read_last_touched_file_limited_checks_size_after_open() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(LAST_TOUCHED_FILE);
        fs::write(&path, "bd-abc123\n").expect("write last touched");
        let metadata = fs::metadata(&path).expect("metadata");
        fs::write(&path, vec![b'a'; MAX_LAST_TOUCHED_BYTES + 1]).expect("grow last touched");

        let value = read_last_touched_file_limited(&path, &metadata).expect("read last touched");

        assert_eq!(value, "");
    }

    #[test]
    fn test_read_last_touched_file_limited_rejects_invalid_utf8() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join(LAST_TOUCHED_FILE);
        fs::write(&path, [0xff]).expect("write invalid last touched");
        let metadata = fs::metadata(&path).expect("metadata");

        let err = read_last_touched_file_limited(&path, &metadata)
            .expect_err("invalid UTF-8 should fail");

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn durable_rename_syncs_parent_once_for_same_directory() {
        let temp = TempDir::new().expect("temp dir");
        let from = temp.path().join("staged");
        let to = temp.path().join("target");
        fs::write(&from, "new").expect("write staged file");

        let mut synced = Vec::new();
        durable_rename_with_parent_sync(&from, &to, |parent| {
            synced.push(parent.to_path_buf());
            Ok(())
        })
        .expect("durable rename");

        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).expect("read target"), "new");
        assert_eq!(synced, vec![temp.path().to_path_buf()]);
    }

    #[test]
    fn durable_rename_syncs_both_parents_for_cross_directory_rename() {
        let temp = TempDir::new().expect("temp dir");
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        fs::create_dir_all(&source_dir).expect("source dir");
        fs::create_dir_all(&target_dir).expect("target dir");
        let from = source_dir.join("staged");
        let to = target_dir.join("target");
        fs::write(&from, "new").expect("write staged file");

        let mut synced = Vec::new();
        durable_rename_with_parent_sync(&from, &to, |parent| {
            synced.push(parent.to_path_buf());
            Ok(())
        })
        .expect("durable rename");

        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).expect("read target"), "new");
        assert_eq!(synced, vec![target_dir, source_dir]);
    }

    #[test]
    fn durable_rename_reports_parent_sync_failure_after_successful_rename() {
        let temp = TempDir::new().expect("temp dir");
        let from = temp.path().join("staged");
        let to = temp.path().join("target");
        fs::write(&from, "new").expect("write staged file");

        let err = durable_rename_with_parent_sync(&from, &to, |_parent| {
            Err(io::Error::other("forced parent fsync failure"))
        })
        .expect_err("parent fsync failure should surface");

        assert_eq!(err.to_string(), "forced parent fsync failure");
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).expect("read target"), "new");
    }
}
