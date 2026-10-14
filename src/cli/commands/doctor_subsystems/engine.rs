//! The `engine` block `br info --json` and `br doctor --json` report: which
//! storage engine this binary was built against, and the on-disk state that
//! incident reports for the August 2026 corruption program had to assemble
//! by hand (sidecar inventory, the sole-opener lease, recovery artifacts).
//!
//! Everything here is observational: files are only stat'ed, and the lease
//! probe takes and immediately releases a non-blocking lock attempt. Recovery
//! advice is not an admission decision or proof that a checkpoint is safe.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Sidecar suffixes FrankenSQLite may leave beside `beads.db`.
const SIDECAR_SUFFIXES: [&str; 8] = [
    "-wal",
    "-shm",
    "-wal-cert",
    "-wal-cert-head",
    "-ns",
    "-journal",
    "-fsqlite-ns-gate",
    "-fsqlite-ns-use",
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
    /// This br lease does not exclude stock SQLite users and is not authority
    /// to recover, replace an index, or checkpoint a database.
    pub held: Option<bool>,
}

/// Advisory next steps for GH #562, never authorization to mutate the family.
/// Metadata cannot establish whether WAL frames are valid or checkpointed.
#[derive(Debug, Clone, Serialize)]
pub struct WalRecoveryAdvice {
    pub code: &'static str,
    pub reason: &'static str,
    pub wal_bytes: u64,
    pub automatic_recovery_supported: bool,
    pub requires_quiescence: bool,
    pub message: &'static str,
    /// Lossless argv, not a shell string. Absent for non-UTF-8 paths or when
    /// an absolute path cannot be obtained; never act on a lossy spelling.
    pub checkpoint_argv: Option<Vec<String>>,
    pub integrity_check_argv: Option<Vec<String>>,
    /// These renderings are for a POSIX shell only. Use argv on other hosts.
    pub command_shell: Option<&'static str>,
    pub checkpoint_command: Option<String>,
    pub integrity_check_command: Option<String>,
    /// sqlite3 can exit successfully even when the checkpoint reports busy.
    pub checkpoint_success_row: &'static str,
    pub integrity_check_success_row: &'static str,
}

const WAL_RECOVERY_MESSAGE: &str = "WAL bytes beyond the header may contain committed, \
    uncheckpointed changes; the missing or short SHM is not permission to discard them. \
    Close ALL br and external SQLite readers/writers and keep them stopped. Preserve \
    the complete database family and rehearse on a disposable copy before using stock \
    sqlite3 on the selected database. Require checkpoint result 0|0|0 (exit status alone \
    is insufficient), then integrity_check result ok and verification of expected records. \
    A busy/error result is not recovery success. Do not delete or replace the WAL or \
    rebuild from JSONL. The br opener-lease probe does not prove quiescence.";

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn posix_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn wal_recovery_advice(
    db_path: &Path,
    engine_reads_on_disk_index: bool,
) -> Option<WalRecoveryAdvice> {
    if !engine_reads_on_disk_index || !std::fs::symlink_metadata(db_path).ok()?.is_file() {
        return None;
    }
    let wal = std::fs::symlink_metadata(sidecar_path(db_path, "-wal")).ok()?;
    if !wal.is_file() || wal.len() <= 32 {
        return None;
    }
    let reason = match std::fs::symlink_metadata(sidecar_path(db_path, "-shm")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing_shm",
        Ok(shm) if shm.is_file() && shm.len() < 96 => "short_shm",
        // Unreadable, non-regular, and initialized-looking files are not a
        // missing index. In particular, do not follow symlinks for this advice.
        Ok(_) | Err(_) => return None,
    };
    // Use the selected --db, never BEADS_DIR or a default filename. Absolute
    // paths also keep leading '-' names from becoming sqlite3 CLI options.
    let database = std::path::absolute(db_path)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned));
    let checkpoint_argv = database.as_ref().map(|database| {
        vec![
            "sqlite3".to_string(),
            "-batch".to_string(),
            "-bail".to_string(),
            database.clone(),
            "PRAGMA wal_checkpoint(TRUNCATE);".to_string(),
        ]
    });
    let integrity_check_argv = database.map(|database| {
        vec![
            "sqlite3".to_string(),
            "-batch".to_string(),
            "-bail".to_string(),
            "-readonly".to_string(),
            database,
            "PRAGMA integrity_check;".to_string(),
        ]
    });
    let render = |argv: &Option<Vec<String>>| {
        if cfg!(unix) {
            argv.as_deref().map(posix_command)
        } else {
            None
        }
    };
    Some(WalRecoveryAdvice {
        code: "WAL_CHECKPOINT_REQUIRED",
        reason,
        wal_bytes: wal.len(),
        automatic_recovery_supported: false,
        requires_quiescence: true,
        message: WAL_RECOVERY_MESSAGE,
        command_shell: if cfg!(unix) && checkpoint_argv.is_some() {
            Some("posix")
        } else {
            None
        },
        checkpoint_command: render(&checkpoint_argv),
        integrity_check_command: render(&integrity_check_argv),
        checkpoint_argv,
        integrity_check_argv,
        checkpoint_success_row: "0|0|0",
        integrity_check_success_row: "ok",
    })
}

/// Engine name, version, and on-disk state.
#[derive(Debug, Clone, Serialize)]
pub struct EngineBlock {
    pub name: &'static str,
    #[serde(rename = "crate")]
    pub crate_name: &'static str,
    /// The `fsqlite` version from Cargo.lock at build time.
    pub version: &'static str,
    pub version: &'static str,
    pub database: String,
    pub sidecars: Vec<EngineFile>,
    pub sole_opener_lease: Option<OpenerLease>,
    pub recovery_artifacts: Vec<EngineFile>,
    /// True when more artifacts exist than are listed.
    pub recovery_artifacts_truncated: bool,
    /// A sampled observation, not a locked snapshot or WAL validation result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_recovery: Option<WalRecoveryAdvice>,
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
    let sidecars = SIDECAR_SUFFIXES
        .iter()
        .filter_map(|suffix| engine_file(&sidecar_path(db_path, suffix)))
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
        wal_recovery: wal_recovery_advice(
            db_path,
            crate::franken_sync::wal_index::ENGINE_READS_ON_DISK_WAL_INDEX,
        ),
    }
}

impl EngineBlock {
    /// Human summary, including actionable advice when recovery is blocked.
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
        let mut summary = format!(
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
        );
        if let Some(advice) = &self.wal_recovery {
            use std::fmt::Write as _;
            let _ = write!(summary, "\n{}: {}", advice.code, advice.message);
            if let Some(command) = &advice.checkpoint_command {
                let _ = write!(summary, "\nCheckpoint (POSIX shell): {command}");
            }
            if let Some(command) = &advice.integrity_check_command {
                let _ = write!(summary, "\nVerify (POSIX shell): {command}");
            }
        }
        summary
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
        std::fs::write(beads_dir.join("beads.db-fsqlite-ns-gate"), b"").unwrap();
        std::fs::write(beads_dir.join("beads.db-fsqlite-ns-use"), b"namespace").unwrap();
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
        assert_eq!(block.sidecars.len(), 3);
        assert!(block.sidecars[0].file.ends_with("beads.db-wal"));
        assert_eq!(block.sidecars[0].bytes, 3);
        assert_eq!(block.sidecars[1].bytes, 0);
        assert!(block.sidecars[1].file.ends_with("-fsqlite-ns-gate"));
        assert_eq!(block.sidecars[2].bytes, 9);
        assert!(block.sidecars[2].file.ends_with("-fsqlite-ns-use"));
        assert_eq!(block.recovery_artifacts.len(), 1);
        assert!(!block.recovery_artifacts_truncated);
        assert!(block.sole_opener_lease.is_none());
        assert!(block.wal_recovery.is_none());
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

    #[test]
    fn wal_advice_is_conservative_and_does_not_open_an_engine() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("selected.db");
        std::fs::write(&db, b"not opened as a database").unwrap();
        let wal = sidecar_path(&db, "-wal");
        assert!(wal_recovery_advice(&db, true).is_none());
        for length in [0, 1, 31, 32] {
            std::fs::write(&wal, vec![0; length]).unwrap();
            assert!(wal_recovery_advice(&db, true).is_none());
        }
        // Even one byte beyond the header must not be described as empty or
        // disposable. This is an observation, NOT a claim of a valid commit.
        std::fs::write(&wal, [0; 33]).unwrap();
        let advice = wal_recovery_advice(&db, true).unwrap();
        assert_eq!(advice.reason, "missing_shm");
        assert_eq!(advice.wal_bytes, 33);
        assert!(!advice.automatic_recovery_supported);
        assert!(advice.requires_quiescence);
        assert_eq!(advice.checkpoint_success_row, "0|0|0");
        assert!(wal_recovery_advice(&db, false).is_none());
        assert!(!sidecar_path(&db, "-shm").exists());
        assert_eq!(std::fs::read(&wal).unwrap(), [0; 33]);
        assert_eq!(std::fs::read(&db).unwrap(), b"not opened as a database");
        for length in [0, 1, 95] {
            std::fs::write(sidecar_path(&db, "-shm"), vec![0; length]).unwrap();
            assert_eq!(wal_recovery_advice(&db, true).unwrap().reason, "short_shm");
        }
        std::fs::write(sidecar_path(&db, "-shm"), [0; 96]).unwrap();
        assert!(wal_recovery_advice(&db, true).is_none());
    }

    #[test]
    fn wal_advice_commands_preserve_selected_path_and_quote_shell_metacharacters() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("-selected ' $HOME ; database.db");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(sidecar_path(&db, "-wal"), [0; 33]).unwrap();
        let advice = wal_recovery_advice(&db, true).unwrap();
        let argv = advice.checkpoint_argv.as_ref().unwrap();
        assert_eq!(Path::new(&argv[3]), std::path::absolute(&db).unwrap());
        assert_eq!(argv[4], "PRAGMA wal_checkpoint(TRUNCATE);");
        assert_eq!(shell_words::split(&posix_command(argv)).unwrap(), *argv);
        let verify = advice.integrity_check_argv.as_ref().unwrap();
        assert_eq!(verify[3], "-readonly");
        assert_eq!(verify[4], argv[3]);
        assert_eq!(shell_words::split(&posix_command(verify)).unwrap(), *verify);
        let json = serde_json::to_value(&advice).unwrap();
        assert_eq!(json["code"], "WAL_CHECKPOINT_REQUIRED");
        assert_eq!(json["checkpoint_argv"][3], argv[3]);
        assert!(advice.message.contains("Do not delete or replace the WAL"));
        assert!(advice.message.contains("exit status alone"));
    }

    #[cfg(unix)]
    #[test]
    fn wal_advice_does_not_follow_a_sidecar_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("selected.db");
        let target = temp.path().join("external");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(&target, [0; 33]).unwrap();
        symlink(&target, sidecar_path(&db, "-wal")).unwrap();
        assert!(wal_recovery_advice(&db, true).is_none());
        assert_eq!(std::fs::read(&target).unwrap(), [0; 33]);
    }

    #[cfg(unix)]
    #[test]
    fn wal_advice_never_builds_a_command_with_a_lossy_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join(OsString::from_vec(b"selected-\xff.db".to_vec()));
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(sidecar_path(&db, "-wal"), [0; 33]).unwrap();
        let advice = wal_recovery_advice(&db, true).unwrap();
        assert!(advice.checkpoint_argv.is_none());
        assert!(advice.integrity_check_argv.is_none());
        assert!(advice.checkpoint_command.is_none());
        assert!(advice.command_shell.is_none());
        assert_eq!(advice.reason, "missing_shm");
    }
}
