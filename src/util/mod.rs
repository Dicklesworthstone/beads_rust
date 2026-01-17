//! Shared utilities for `beads_rust`.
//!
//! Common functionality used across modules:
//! - Content hashing (SHA256)
//! - Time parsing and formatting (RFC3339)
//! - Path handling (.beads discovery)
//! - ID generation (base36 adaptive)
//! - Last-touched tracking

mod hash;
pub mod id;
pub mod markdown_import;
pub mod time;

pub use hash::{ContentHashable, content_hash, content_hash_from_parts};
pub use id::{
    IdConfig, IdGenerator, IdResolver, MatchType, ParsedId, ResolvedId, ResolverConfig, child_id,
    find_matching_ids, generate_id, id_depth, is_child_id, is_valid_id_format, normalize_id,
    parse_id, resolve_id, validate_prefix,
};

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LAST_TOUCHED_FILE: &str = "last-touched";

/// Build the path to `.beads/last-touched` from the beads directory.
#[must_use]
pub fn last_touched_path(beads_dir: &Path) -> PathBuf {
    beads_dir.join(LAST_TOUCHED_FILE)
}

/// Best-effort write of the last-touched issue ID.
///
/// Errors are ignored to match classic bd behavior.
pub fn set_last_touched_id(beads_dir: &Path, id: &str) {
    let path = last_touched_path(beads_dir);
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
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
}
