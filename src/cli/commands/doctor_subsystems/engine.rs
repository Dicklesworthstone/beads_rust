//! The `engine` block `br info --json` and `br doctor --json` report: which
//! storage engine this binary was built against, and the on-disk state that
//! incident reports for the August 2026 corruption program had to assemble
//! by hand (sidecar inventory, the sole-opener lease, recovery artifacts).
//!
//! Everything here is observational: files are only stat'ed, and the lease
//! probe takes and immediately releases a non-blocking lock attempt.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Sidecar suffixes FrankenSQLite may leave beside `beads.db`.
const SIDECAR_SUFFIXES: [&str; 6] = [
    "-wal",
    "-shm",
    "-wal-cert",
    "-wal-cert-head",
    "-ns",
    "-journal",
];

/// Recovery artifacts listed at most this many entries deep.
const RECOVERY_ARTIFACT_LIMIT: usize = 50;

/// One file beside the database.
#[derive(Debug, Clone, Serialize)]
pub struct EngineFile {
    pub file: String,
    pub bytes: u64,
    /// Seconds since last modification; `None` when the clock cannot say.
    pub age_s: Option<u64>,
}

/// The shared opener lease file (`.br-db-openers-<hash>.lock`).
#[derive(Debug, Clone, Serialize)]
pub struct OpenerLease {
    pub file: String,
    pub age_s: Option<u64>,
    /// `Some(true)` when some open connection holds the lease (a
    /// non-blocking exclusive lock attempt on a fresh descriptor was
    /// refused; this process's own connection counts), `Some(false)` when
    /// nobody does, `None` when the probe is unavailable on this platform.
    pub held: Option<bool>,
}

/// Engine name, version, and on-disk state.
#[derive(Debug, Clone, Serialize)]
pub struct EngineBlock {
    pub name: &'static str,
    #[serde(rename = "crate")]
    pub crate_name: &'static str,
    /// The `fsqlite` version from Cargo.lock at build time.
    pub version: &'static str,
    pub database: String,
    pub sidecars: Vec<EngineFile>,
    pub sole_opener_lease: Option<OpenerLease>,
    pub recovery_artifacts: Vec<EngineFile>,
    /// True when more artifacts exist than are listed.
    pub recovery_artifacts_truncated: bool,
}

/// The `fsqlite` version this binary was built with.
#[must_use]
pub const fn engine_version() -> &'static str {
    match option_env!("BR_FSQLITE_VERSION") {
        Some(version) => version,
        None => "unknown",
    }
}

fn age_seconds(metadata: &std::fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age.as_secs())
}

fn engine_file(path: &Path) -> Option<EngineFile> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(EngineFile {
        file: path.display().to_string(),
        bytes: metadata.len(),
        age_s: age_seconds(&metadata),
    })
}

#[cfg(unix)]
fn probe_lease_held(path: &Path) -> Option<bool> {
    use rustix::fs::{FlockOperation, flock};
    let file = std::fs::File::open(path).ok()?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = flock(&file, FlockOperation::Unlock);
            Some(false)
        }
        Err(err) if err == rustix::io::Errno::WOULDBLOCK => Some(true),
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn probe_lease_held(_path: &Path) -> Option<bool> {
    None
}

fn opener_lease(db_path: &Path) -> Option<OpenerLease> {
    let parent = db_path.parent()?;
    let entries = std::fs::read_dir(parent).ok()?;
    let mut leases: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".br-db-openers-"))
        })
        .collect();
    leases.sort();
    let path = leases.into_iter().next()?;
    let age_s = std::fs::metadata(&path)
        .ok()
        .and_then(|metadata| age_seconds(&metadata));
    Some(OpenerLease {
        file: path.display().to_string(),
        age_s,
        held: probe_lease_held(&path),
    })
}

fn recovery_artifacts(beads_dir: &Path) -> (Vec<EngineFile>, bool) {
    let root = beads_dir.join(".br_recovery");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return (Vec::new(), false);
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                std::fs::read_dir(&path)
                    .map(|inner| {
                        inner
                            .filter_map(Result::ok)
                            .map(|child| child.path())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|_| vec![path])
            } else {
                vec![path]
            }
        })
        .collect();
    paths.sort();
    let truncated = paths.len() > RECOVERY_ARTIFACT_LIMIT;
    paths.truncate(RECOVERY_ARTIFACT_LIMIT);
    (
        paths.iter().filter_map(|path| engine_file(path)).collect(),
        truncated,
    )
}

/// Build the engine block for a workspace.
#[must_use]
pub fn engine_block(beads_dir: &Path, db_path: &Path) -> EngineBlock {
    let db_string = db_path.to_string_lossy();
    let sidecars = SIDECAR_SUFFIXES
        .iter()
        .filter_map(|suffix| engine_file(&PathBuf::from(format!("{db_string}{suffix}"))))
        .collect();
    let (recovery_artifacts, recovery_artifacts_truncated) = recovery_artifacts(beads_dir);
    EngineBlock {
        name: "frankensqlite",
        crate_name: "fsqlite",
        version: engine_version(),
        database: db_path.display().to_string(),
        sidecars,
        sole_opener_lease: opener_lease(db_path),
        recovery_artifacts,
        recovery_artifacts_truncated,
    }
}

impl EngineBlock {
    /// One-line human summary.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let sidecars = if self.sidecars.is_empty() {
            "none".to_string()
        } else {
            self.sidecars
                .iter()
                .filter_map(|sidecar| {
                    Path::new(&sidecar.file)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let lease = match &self.sole_opener_lease {
            None => "no lease file".to_string(),
            Some(lease) => match lease.held {
                Some(true) => "lease held".to_string(),
                Some(false) => "lease free".to_string(),
                None => "lease present".to_string(),
            },
        };
        format!(
            "{} {} ({}); sidecars: {sidecars}; {lease}; recovery artifacts: {}{}",
            self.name,
            self.version,
            self.crate_name,
            self.recovery_artifacts.len(),
            if self.recovery_artifacts_truncated {
                "+"
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_block_lists_existing_sidecars_and_artifacts_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let beads_dir = temp.path();
        let db = beads_dir.join("beads.db");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(beads_dir.join("beads.db-wal"), b"wal").unwrap();
        std::fs::create_dir_all(beads_dir.join(".br_recovery").join("run-1")).unwrap();
        std::fs::write(
            beads_dir
                .join(".br_recovery")
                .join("run-1")
                .join("beads.db"),
            b"old",
        )
        .unwrap();

        let block = engine_block(beads_dir, &db);
        assert_eq!(block.name, "frankensqlite");
        assert_eq!(block.crate_name, "fsqlite");
        assert!(!block.version.is_empty());
        assert_eq!(block.sidecars.len(), 1);
        assert!(block.sidecars[0].file.ends_with("beads.db-wal"));
        assert_eq!(block.sidecars[0].bytes, 3);
        assert_eq!(block.recovery_artifacts.len(), 1);
        assert!(!block.recovery_artifacts_truncated);
        assert!(block.sole_opener_lease.is_none());
        let line = block.summary_line();
        assert!(
            line.contains("beads.db-wal") && line.contains("no lease file"),
            "{line}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn opener_lease_probe_reports_held_and_free() {
        use rustix::fs::{FlockOperation, flock};
        let temp = tempfile::tempdir().expect("tempdir");
        let db = temp.path().join("beads.db");
        std::fs::write(&db, b"db").unwrap();
        let lease_path = temp.path().join(".br-db-openers-abcdef.lock");
        std::fs::write(&lease_path, b"").unwrap();

        let free = engine_block(temp.path(), &db);
        assert_eq!(
            free.sole_opener_lease.as_ref().and_then(|lease| lease.held),
            Some(false)
        );

        let holder = std::fs::File::open(&lease_path).unwrap();
        flock(&holder, FlockOperation::LockShared).unwrap();
        let held = engine_block(temp.path(), &db);
        assert_eq!(
            held.sole_opener_lease.as_ref().and_then(|lease| lease.held),
            Some(true)
        );
        assert!(held.summary_line().contains("lease held"));
    }
}
