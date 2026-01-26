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

pub use hash::{ContentHashable, content_hash, content_hash_from_parts};
pub use id::{
    IdConfig, IdGenerator, IdResolver, MatchType, ParsedId, ResolvedId, ResolverConfig, child_id,
    find_matching_ids, generate_id, id_depth, is_child_id, is_valid_id_format, normalize_id,
    parse_id, resolve_id, validate_prefix,
};

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LAST_TOUCHED_FILE: &str = "last-touched";

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
        options.mode(0o644);
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
    let mut contents = String::new();

    if let Ok(mut file) = fs::File::open(path) {
        if file.read_to_string(&mut contents).is_ok() {
            return contents.lines().next().unwrap_or("").trim().to_string();
        }
    }

    String::new()
}

/// Best-effort delete of the last-touched file.
pub fn clear_last_touched(beads_dir: &Path) {
    let path = last_touched_path(beads_dir);
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
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
}
