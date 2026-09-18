//! Narrow containment for the initialized, zero-page WAL index in GitHub #507.
//!
//! This is not a general SQLite repairer. The writable identity-bound recovery
//! open may quarantine this exact derived-cache signature, but only after
//! acquiring both the engine namespace and SQLite's main-file lock range.
//! Main/WAL/journal/certificate files are never rewritten or removed here.
//! The surrounding doctor recovery workflow still owns the complete backup,
//! private rehearsal, protected-byte comparison, and logical attestation.
//!
//! A read-only open only reports an advisory diagnosis. BusyRecovery by itself,
//! differing salts, and a missing/short index are NOT quarantine authority.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use fsqlite_vfs::FileIdentity;
use fsqlite_vfs::namespace::{NamespaceOpenIntent, PendingNamespaceOpen};
use sha2::{Digest, Sha256};

use super::FrankenError;

/// Owns a SQLite-compatible exclusive main-file range lock. Its platform
/// constructor lives in sync::db_inode_lock, the existing syscall boundary.
/// Closing this owned descriptor releases its OFD lock; unrelated closes do not.
pub(crate) struct RecoveryLock {
    pub(crate) file: File,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn open_regular(path: &Path, writable: bool) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("WAL-index recovery refuses non-regular files"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(
            windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
        );
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid("WAL-index recovery descriptor is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid("WAL-index recovery refuses hard-linked files"));
        }
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<FileIdentity> {
    FileIdentity::from_file(file)?
        .ok_or_else(|| invalid("WAL-index recovery cannot verify the file identity"))
}

fn verify_name(path: &Path, retained: &File) -> io::Result<()> {
    if identity(&open_regular(path, false)?)? != identity(retained)? {
        return Err(invalid("WAL-index recovery path changed identity"));
    }
    Ok(())
}

fn prefix<const N: usize>(path: &Path) -> io::Result<Option<[u8; N]>> {
    let result: io::Result<[u8; N]> = (|| {
        let mut bytes = [0; N];
        open_regular(path, false)?.read_exact(&mut bytes)?;
        Ok(bytes)
    })();
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("fixed-width WAL field"))
}

fn checksum(bytes: &[u8], mut state: [u32; 2], big_endian: bool) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        let word = |bytes: &[u8]| {
            let bytes = bytes.try_into().expect("fixed-width checksum word");
            if big_endian {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            }
        };
        state[0] = state[0].wrapping_add(word(&pair[..4])).wrapping_add(state[1]);
        state[1] = state[1].wrapping_add(word(&pair[4..])).wrapping_add(state[0]);
    }
    state
}

fn wal_layout(header: &[u8; 32]) -> io::Result<(u32, bool)> {
    let magic = be32(&header[..4]);
    let page_size = be32(&header[8..12]);
    if !matches!(magic, 0x377f_0682 | 0x377f_0683)
        || be32(&header[4..8]) != 3_007_000
        || !(512..=65_536).contains(&page_size)
        || !page_size.is_power_of_two()
    {
        return Err(invalid("invalid WAL header; refusing index quarantine"));
    }
    let big_endian = magic & 1 != 0;
    if checksum(&header[..24], [0, 0], big_endian)
        != [be32(&header[24..28]), be32(&header[28..32])]
    {
        return Err(invalid("invalid WAL header checksum; refusing index quarantine"));
    }
    Ok((page_size, big_endian))
}

fn poisoned_headers(wal: &[u8; 32], shm: &[u8; 96]) -> bool {
    // The WAL-index scalars are native endian; the salts are raw bytes copied
    // from the big-endian WAL header. Never compare native-decoded index salts
    // with big-endian-decoded WAL salts (a healthy little-endian index differs).
    let version = u32::from_ne_bytes(shm[..4].try_into().expect("version field"));
    wal_layout(wal).is_ok()
        && matches!(version, 0 | 3_007_000)
        && shm[..48] == shm[48..]
        && shm[12] == 1
        && shm[14..24].iter().all(|byte| *byte == 0)
        && shm[32..40].iter().all(|byte| *byte == 0)
}

fn probe(path: &Path) -> io::Result<bool> {
    let Some(wal) = prefix::<32>(&sidecar(path, "-wal"))? else {
        return Ok(false);
    };
    let Some(shm) = prefix::<96>(&sidecar(path, "-shm"))? else {
        return Ok(false);
    };
    Ok(poisoned_headers(&wal, &shm))
}

pub(super) fn warn_if_poisoned(path: &str) {
    if probe(Path::new(path)).unwrap_or(false) {
        tracing::warn!(
            database = path,
            diagnostic = "WAL_INDEX_POISONED",
            "initialized zero-page WAL index observed beside a valid WAL header; \
             close database users, preserve the complete family, and run \
             `br doctor migrate-schema recover` on a supported native Unix host. \
             Do not rebuild from potentially stale JSONL or delete the WAL"
        );
    }
}

/// Conservative validation: require a complete checksummed WAL ending at a
/// commit (or a header-only WAL). A partial, corrupt, or old-generation tail
/// is retained for engine-level recovery, never silently discarded here.
fn validate_wal(main: &mut File, wal: &mut File) -> io::Result<()> {
    let mut database_header = [0; 100];
    main.rewind()?;
    main.read_exact(&mut database_header)?;
    if &database_header[..16] != b"SQLite format 3\0" {
        return Err(invalid("invalid main database header"));
    }
    let encoded = u16::from_be_bytes([database_header[16], database_header[17]]);
    let database_page_size = if encoded == 1 {
        65_536
    } else {
        u32::from(encoded)
    };
    let mut header = [0; 32];
    wal.rewind()?;
    wal.read_exact(&mut header)?;
    let (page_size, big_endian) = wal_layout(&header)?;
    if page_size != database_page_size {
        return Err(invalid("WAL and main database page sizes differ"));
    }
    let frame_size = u64::from(page_size) + 24;
    let length = wal.metadata()?.len();
    if length < 32 || (length - 32) % frame_size != 0 {
        return Err(invalid("partial WAL frame; refusing index quarantine"));
    }
    let mut frame = vec![0; usize::try_from(frame_size).map_err(|_| invalid("WAL frame size"))?];
    let mut state = [be32(&header[24..28]), be32(&header[28..32])];
    let mut committed = true;
    for _ in 0..(length - 32) / frame_size {
        wal.read_exact(&mut frame)?;
        if frame[8..16] != header[16..24] || matches!(be32(&frame[..4]), 0 | u32::MAX) {
            return Err(invalid("invalid WAL frame binding; refusing index quarantine"));
        }
        state = checksum(&frame[..8], state, big_endian);
        state = checksum(&frame[24..], state, big_endian);
        if state != [be32(&frame[16..20]), be32(&frame[20..24])] {
            return Err(invalid("invalid WAL frame checksum; refusing index quarantine"));
        }
        committed = be32(&frame[4..8]) != 0;
    }
    if !committed {
        return Err(invalid("uncommitted WAL tail; refusing index quarantine"));
    }
    Ok(())
}

fn hash_file(file: &mut File) -> io::Result<String> {
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..count]);
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory synchronization unavailable",
        ))
    }
}

fn quarantine_name(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        // No unlink fallback: a filesystem without atomic no-replace support
        // must retain the live cache instead of risking an evidence overwrite.
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = (from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic WAL-index quarantine unavailable",
        ))
    }
}

/// Only the identity-bound writable recovery open calls this function. It
/// returns false for every signature outside #507, preserving BusyRecovery's
/// ordinary contention meaning. Failure after quarantine leaves a missing
/// regenerable cache and retained evidence, never reinstalls poisoned state.
#[allow(clippy::too_many_lines)]
pub(super) fn quarantine_poisoned_index(
    path: &str,
    expected_identity: FileIdentity,
) -> Result<bool, FrankenError> {
    let requested = Path::new(path);
    if !probe(requested)? {
        return Ok(false);
    }
    let parent = requested
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let path = parent.join(
        requested
            .file_name()
            .ok_or_else(|| invalid("missing database filename"))?,
    );
    let main = open_regular(&path, true)?;
    if identity(&main)? != expected_identity {
        return Err(invalid("recovery database no longer matches the retained identity").into());
    }
    // Shared admission obtains both locks exclusively for a quiescent
    // namespace; expected_identity=Some means we joined a live peer instead.
    let pending = PendingNamespaceOpen::begin(&path, NamespaceOpenIntent::Shared)?;
    if pending.expected_identity().is_some() {
        return Err(FrankenError::BusyRecovery);
    }
    let mut locked = RecoveryLock::acquire(main).map_err(|error| match error {
        TryLockError::WouldBlock => FrankenError::BusyRecovery,
        TryLockError::Error(error) => error.into(),
    })?;
    verify_name(&path, &locked.file)?;
    let _namespace = if pending.has_quiescent_record_bytes()? {
        pending.bind_replacing_quiescent_record(expected_identity)?
    } else {
        pending.bind(expected_identity)?
    };
    let wal_path = sidecar(&path, "-wal");
    let shm_path = sidecar(&path, "-shm");
    let mut wal = open_regular(&wal_path, false)?;
    let mut shm = open_regular(&shm_path, true)?;
    let mut wal_header = [0; 32];
    let mut shm_headers = [0; 96];
    wal.read_exact(&mut wal_header)?;
    shm.read_exact(&mut shm_headers)?;
    if !poisoned_headers(&wal_header, &shm_headers) {
        return Ok(false);
    }
    validate_wal(&mut locked.file, &mut wal)?;
    let main_hash = hash_file(&mut locked.file)?;
    let wal_hash = hash_file(&mut wal)?;
    let shm_hash = hash_file(&mut shm)?;
    // Keep, rather than auto-delete, even if a later fsync/rename/open fails.
    let retained = tempfile::Builder::new()
        .prefix(".br-wal-index-")
        .tempdir_in(&parent)?
        .keep();
    let destination = retained.join("poisoned-shm");
    let operation = (|| -> io::Result<()> {
        let receipt = serde_json::json!({
            "schema_version": "br.wal_index.quarantine.v1",
            "database_path": path.display().to_string(),
            "reason": "initialized-zero-page-wal-index",
            "main_sha256": &main_hash,
            "wal_sha256": &wal_hash,
            "shm_sha256": &shm_hash,
            "retained_index": destination.display().to_string(),
        });
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(retained.join("prepared.json"))?;
        marker.write_all(
            serde_json::to_string_pretty(&receipt)
                .map_err(io::Error::other)?
                .as_bytes(),
        )?;
        marker.sync_all()?;
        shm.sync_all()?;
        sync_directory(&retained)?;
        sync_directory(&parent)?;
        #[cfg(test)]
        tests::crash_at_recovery_boundary("prepared");
        verify_name(&path, &locked.file)?;
        verify_name(&wal_path, &wal)?;
        verify_name(&shm_path, &shm)?;
        // Even inside this call's private directory, never clobber an entry.
        quarantine_name(&shm_path, &destination)?;
        #[cfg(test)]
        tests::crash_at_recovery_boundary("renamed");
        sync_directory(&retained)?;
        sync_directory(&parent)?;
        verify_name(&destination, &shm)?;
        if hash_file(&mut locked.file)? != main_hash
            || hash_file(&mut wal)? != wal_hash
            || hash_file(&mut shm)? != shm_hash
        {
            return Err(invalid(
                "recovery payload changed; inspect retained evidence before retrying",
            ));
        }
        #[cfg(test)]
        tests::crash_at_recovery_boundary("durable");
        Ok(())
    })();
    operation.map_err(|error| {
        FrankenError::internal(format!(
            "WAL-index quarantine did not complete: {error}; evidence retained at {}",
            retained.display()
        ))
    })?;
    tracing::warn!(
        database = %path.display(),
        retained = %retained.display(),
        "quarantined poisoned WAL index; main database and WAL preserved byte-for-byte"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    use crate::franken_sync::{Connection, SqliteValue, compat};
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    use crate::franken_sync::tests::run_recovery_test_in_subprocess;

    const CRASH_STAGE_ENV: &str = "BR_TEST_507_CRASH_STAGE";

    // Only compiled into the unit-test binary. exit() intentionally bypasses
    // every Rust destructor: this tests restart after process loss, not an
    // ordinary error return with a conveniently running cleanup path.
    pub(super) fn crash_at_recovery_boundary(stage: &str) {
        if std::env::var(CRASH_STAGE_ENV).as_deref() == Ok(stage) {
            std::process::exit(86);
        }
    }

    fn wal_header(page_size: u32, big_endian: bool) -> [u8; 32] {
        let mut header = [0; 32];
        let magic: u32 = if big_endian { 0x377f_0683 } else { 0x377f_0682 };
        header[..4].copy_from_slice(&magic.to_be_bytes());
        header[4..8].copy_from_slice(&3_007_000_u32.to_be_bytes());
        header[8..12].copy_from_slice(&page_size.to_be_bytes());
        header[16..24].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        let sum = checksum(&header[..24], [0, 0], big_endian);
        header[24..28].copy_from_slice(&sum[0].to_be_bytes());
        header[28..32].copy_from_slice(&sum[1].to_be_bytes());
        header
    }

    fn poison() -> [u8; 96] {
        let mut header = [0; 48];
        header[..4].copy_from_slice(&3_007_000_u32.to_ne_bytes());
        header[12] = 1;
        let mut both = [0; 96];
        both[..48].copy_from_slice(&header);
        both[48..].copy_from_slice(&header);
        both
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, FileIdentity) {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("beads.db");
        let mut main = vec![0; 4096];
        main[..16].copy_from_slice(b"SQLite format 3\0");
        main[16..18].copy_from_slice(&4096_u16.to_be_bytes());
        fs::write(&db, main).unwrap();
        fs::write(sidecar(&db, "-wal"), wal_header(4096, false)).unwrap();
        fs::write(sidecar(&db, "-shm"), poison()).unwrap();
        let id = identity(&File::open(&db).unwrap()).unwrap();
        (temp, db, id)
    }

    fn payload(db: &Path) -> [Vec<u8>; 3] {
        ["", "-wal", "-shm"].map(|suffix| fs::read(sidecar(db, suffix)).unwrap())
    }

    #[test]
    fn recognizes_only_duplicate_initialized_zero_page_headers() {
        for big_endian in [false, true] {
            let wal = wal_header(4096, big_endian);
            assert!(poisoned_headers(&wal, &poison()));
            for offset in [12, 14, 16, 20, 32] {
                let mut changed = poison();
                changed[offset] ^= 1;
                changed[offset + 48] ^= 1;
                assert!(!poisoned_headers(&wal, &changed), "field {offset}");
            }
            let mut torn = poison();
            torn[48] ^= 1;
            assert!(!poisoned_headers(&wal, &torn));
        }
    }

    #[test]
    fn healthy_salts_and_64k_encoding_are_not_poison() {
        for size in [512_u32, 4096, 65_536] {
            let wal = wal_header(size, false);
            let mut shm = poison();
            let encoded = if size == 65_536 { 1 } else { u16::try_from(size).unwrap() };
            for offset in [0, 48] {
                shm[offset + 14..offset + 16].copy_from_slice(&encoded.to_ne_bytes());
                // Salts must retain the WAL byte order, not native scalar order.
                shm[offset + 32..offset + 40].copy_from_slice(&wal[16..24]);
            }
            assert!(!poisoned_headers(&wal, &shm));
            shm[32] ^= 1;
            shm[80] ^= 1;
            assert!(!poisoned_headers(&wal, &shm), "salt mismatch alone is not authority");
        }
    }

    #[test]
    fn invalid_wal_headers_are_not_quarantine_evidence() {
        let header = wal_header(4096, false);
        for offset in [0, 4, 8, 16, 24, 31] {
            let mut damaged = header;
            damaged[offset] ^= 1;
            assert!(!poisoned_headers(&damaged, &poison()));
        }
        for size in [0, 1, 256, 513, 131_072] {
            assert!(wal_layout(&wal_header(size, false)).is_err());
        }
    }

    #[test]
    fn probe_is_bounded_and_observational() {
        let (_temp, db, _) = fixture();
        let before = payload(&db);
        assert!(probe(&db).unwrap());
        assert_eq!(payload(&db), before);
        for bytes in [Vec::new(), vec![0; 31]] {
            fs::write(sidecar(&db, "-wal"), bytes).unwrap();
            assert!(!probe(&db).unwrap());
        }
        assert!(!probe(&db.with_file_name("absent.db")).unwrap());
        fs::write(sidecar(&db, "-wal"), wal_header(4096, false)).unwrap();
        fs::write(sidecar(&db, "-shm"), [0; 95]).unwrap();
        assert!(!probe(&db).unwrap());
    }

    fn one_frame_wal(commit: bool, big_endian: bool) -> Vec<u8> {
        let header = wal_header(4096, big_endian);
        let mut frame = vec![0; 24 + 4096];
        frame[..4].copy_from_slice(&1_u32.to_be_bytes());
        frame[4..8].copy_from_slice(&u32::from(commit).to_be_bytes());
        frame[8..16].copy_from_slice(&header[16..24]);
        let state = checksum(&frame[..8], [be32(&header[24..28]), be32(&header[28..32])], big_endian);
        let state = checksum(&frame[24..], state, big_endian);
        frame[16..20].copy_from_slice(&state[0].to_be_bytes());
        frame[20..24].copy_from_slice(&state[1].to_be_bytes());
        [header.to_vec(), frame].concat()
    }

    #[test]
    fn strict_wal_validation_rejects_corrupt_partial_and_uncommitted_tails() {
        let (_temp, db, _) = fixture();
        let wal_path = sidecar(&db, "-wal");
        for big_endian in [false, true] {
            let valid = one_frame_wal(true, big_endian);
            fs::write(&wal_path, &valid).unwrap();
            assert!(validate_wal(&mut File::open(&db).unwrap(), &mut File::open(&wal_path).unwrap()).is_ok());
            let mut corrupt = valid.clone();
            corrupt[56] ^= 1;
            let mut wrong_salt = valid.clone();
            wrong_salt[40] ^= 1;
            let partial = valid[..valid.len() - 1].to_vec();
            for invalid in [corrupt, wrong_salt, partial, one_frame_wal(false, big_endian)] {
                fs::write(&wal_path, &invalid).unwrap();
                assert!(validate_wal(&mut File::open(&db).unwrap(), &mut File::open(&wal_path).unwrap()).is_err());
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn quarantine_retains_cache_and_preserves_every_payload_byte() {
        let (_temp, db, id) = fixture();
        let before = payload(&db);
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), id).unwrap());
        assert_eq!(fs::read(&db).unwrap(), before[0]);
        assert_eq!(fs::read(sidecar(&db, "-wal")).unwrap(), before[1]);
        assert!(!sidecar(&db, "-shm").exists());
        let retained = fs::read_dir(db.parent().unwrap()).unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(".br-wal-index-"))
            .unwrap();
        assert_eq!(fs::read(retained.join("poisoned-shm")).unwrap(), before[2]);
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(retained.join("prepared.json")).unwrap()).unwrap();
        assert_eq!(receipt["schema_version"], "br.wal_index.quarantine.v1");
        // Crash after quarantine is restartable: no second quarantine is needed.
        assert!(!quarantine_poisoned_index(db.to_str().unwrap(), id).unwrap());
    }

    #[test]
    fn replacement_identity_and_live_engine_namespace_refuse_without_payload_changes() {
        let (_temp, db, id) = fixture();
        let before = payload(&db);
        let peer = PendingNamespaceOpen::begin(&db, NamespaceOpenIntent::ReservedExclusive).unwrap();
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), id).is_err());
        assert_eq!(payload(&db), before);
        drop(peer);
        let other = db.with_file_name("other.db");
        fs::write(&other, &before[0]).unwrap();
        let wrong_id = identity(&File::open(other).unwrap()).unwrap();
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), wrong_id).is_err());
        assert_eq!(payload(&db), before);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_and_hardlinked_index_is_never_quarantined() {
        use std::os::unix::fs::symlink;
        let (_temp, db, id) = fixture();
        let shm = sidecar(&db, "-shm");
        let retained = db.with_file_name("original-index");
        fs::rename(&shm, &retained).unwrap();
        symlink(&retained, &shm).unwrap();
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), id).is_err());
        assert_eq!(fs::read(&retained).unwrap(), poison());
        // Preserve the symlink as evidence rather than deleting it in the test.
        fs::rename(&shm, db.with_file_name("retained-symlink")).unwrap();
        fs::hard_link(&retained, &shm).unwrap();
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), id).is_err());
        assert_eq!(fs::read(&retained).unwrap(), poison());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn invalid_wal_tail_refuses_before_quarantining_index() {
        let (_temp, db, id) = fixture();
        let mut wal = one_frame_wal(true, false);
        wal[56] ^= 1;
        fs::write(sidecar(&db, "-wal"), wal).unwrap();
        let before = payload(&db);
        assert!(quarantine_poisoned_index(db.to_str().unwrap(), id).is_err());
        assert_eq!(payload(&db), before);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    fn tracker_with_uncheckpointed_rows() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("tracker.db");
        let mut connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        connection.execute("PRAGMA journal_mode = WAL").unwrap();
        connection.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        connection.execute("CREATE TABLE issues (id TEXT PRIMARY KEY)").unwrap();
        connection.execute("CREATE TABLE dependencies (source TEXT, target TEXT)").unwrap();
        connection.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)").unwrap();
        connection.execute("PRAGMA user_version = 19").unwrap();
        connection.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        connection.execute("INSERT INTO issues VALUES ('db-only-a'), ('db-only-b'), ('db-only-c')").unwrap();
        connection.execute("INSERT INTO dependencies VALUES ('db-only-a', 'db-only-b'), ('db-only-b', 'db-only-c')").unwrap();
        connection.execute("INSERT INTO metadata VALUES ('sync_merge_pending', 'must-remain-blocking')").unwrap();
        connection.close_without_checkpoint_in_place().unwrap();
        drop(connection);
        assert!(fs::metadata(sidecar(&db, "-wal")).unwrap().len() > 32);
        (temp, db)
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    fn poison_tracker(db: &Path) {
        let mut shm = open_regular(&sidecar(db, "-shm"), true).unwrap();
        shm.write_all(&poison()).unwrap();
        shm.sync_all().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    fn assert_tracker_contents(connection: &Connection) {
        assert_eq!(connection.query("SELECT id FROM issues").unwrap().len(), 3);
        assert_eq!(connection.query("SELECT source FROM dependencies").unwrap().len(), 2);
        let row = connection.query_row("SELECT value FROM metadata WHERE key = 'sync_merge_pending'").unwrap();
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("must-remain-blocking"));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    fn recover_and_close_without_payload_changes(db: &Path) {
        let main_before = fs::read(db).unwrap();
        let wal_before = fs::read(sidecar(db, "-wal")).unwrap();
        let retained = File::open(db).unwrap();
        let recovered = Connection::open_existing_with_expected_identity(
            db.to_string_lossy().into_owned(), identity(&retained).unwrap(),
        ).unwrap();
        assert_tracker_contents(&recovered);
        recovered.close().unwrap();
        assert_eq!(fs::read(db).unwrap(), main_before);
        assert_eq!(fs::read(sidecar(db, "-wal")).unwrap(), wal_before);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn identity_bound_recovery_preserves_unexported_rows_and_pending_metadata() {
        if run_recovery_test_in_subprocess(
            "franken_sync::wal_index::tests::identity_bound_recovery_preserves_unexported_rows_and_pending_metadata",
        ) {
            return;
        }
        let (_temp, db) = tracker_with_uncheckpointed_rows();
        poison_tracker(&db);
        let before = payload(&db);
        // Read-only admission must never quarantine or modify the family.
        match compat::open_with_flags(db.to_str().unwrap(), compat::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Err(FrankenError::BusyRecovery) => {}
            Ok(mut readonly) => {
                assert!(matches!(readonly.query("SELECT id FROM issues"), Err(FrankenError::BusyRecovery)));
                readonly.close_without_checkpoint_in_place().unwrap();
            }
            Err(error) => panic!("unexpected read-only error: {error}"),
        }
        assert_eq!(payload(&db), before);
        let retained = File::open(&db).unwrap();
        let recovered = Connection::open_existing_with_expected_identity(
            db.to_string_lossy().into_owned(), identity(&retained).unwrap(),
        ).unwrap();
        assert_eq!(recovered.query("SELECT id FROM issues").unwrap().len(), 3);
        assert_eq!(recovered.query("SELECT source FROM dependencies").unwrap().len(), 2);
        let row = recovered.query_row("SELECT value FROM metadata WHERE key = 'sync_merge_pending'").unwrap();
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("must-remain-blocking"));
        assert_eq!(fs::read(&db).unwrap(), before[0]);
        assert_eq!(fs::read(sidecar(&db, "-wal")).unwrap(), before[1]);
        // Doctor calls close(), not the special no-checkpoint method. A
        // reconstructed-cache handle must preserve WAL bytes on that path too.
        recovered.close().unwrap();
        assert_eq!(fs::read(&db).unwrap(), before[0]);
        assert_eq!(fs::read(sidecar(&db, "-wal")).unwrap(), before[1]);
        let mut writer = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        writer.execute("INSERT INTO issues VALUES ('writes-work-again')").unwrap();
        writer.close_without_checkpoint_in_place().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn missing_and_healthy_index_recovery_preserve_wal_on_every_close() {
        if run_recovery_test_in_subprocess(
            "franken_sync::wal_index::tests::missing_and_healthy_index_recovery_preserve_wal_on_every_close",
        ) {
            return;
        }
        for missing in [false, true] {
            let (_temp, db) = tracker_with_uncheckpointed_rows();
            if missing {
                fs::rename(sidecar(&db, "-shm"), db.with_file_name("retained-shm")).unwrap();
            }
            // This invocation did not quarantine anything. A second recovery
            // also must not checkpoint the WAL merely because the first one
            // successfully rebuilt the index.
            recover_and_close_without_payload_changes(&db);
            recover_and_close_without_payload_changes(&db);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    #[ignore = "subprocess worker for recovery_survives_abrupt_process_exit"]
    fn recovery_crash_worker() {
        let db = PathBuf::from(std::env::var_os("BR_TEST_507_CRASH_DATABASE").unwrap());
        let retained = File::open(&db).unwrap();
        let recovered = Connection::open_existing_with_expected_identity(
            db.to_string_lossy().into_owned(), identity(&retained).unwrap(),
        ).unwrap();
        assert_tracker_contents(&recovered);
        crash_at_recovery_boundary("admitted");
        recovered.close().unwrap();
        panic!("requested crash boundary was not reached");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    #[test]
    fn recovery_survives_abrupt_process_exit() {
        if run_recovery_test_in_subprocess(
            "franken_sync::wal_index::tests::recovery_survives_abrupt_process_exit",
        ) {
            return;
        }
        const WORKER: &str = "franken_sync::wal_index::tests::recovery_crash_worker";
        for stage in ["prepared", "renamed", "durable", "admitted"] {
            let (_temp, db) = tracker_with_uncheckpointed_rows();
            poison_tracker(&db);
            let before = payload(&db);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", WORKER, "--ignored", "--nocapture", "--test-threads=1"])
                .env("BR_TEST_507_CRASH_DATABASE", &db)
                .env(CRASH_STAGE_ENV, stage)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(), Some(86),
                "worker did not reach {stage}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert_eq!(fs::read(&db).unwrap(), before[0], "main at {stage}");
            assert_eq!(fs::read(sidecar(&db, "-wal")).unwrap(), before[1], "WAL at {stage}");
            match stage {
                "prepared" => assert_eq!(fs::read(sidecar(&db, "-shm")).unwrap(), before[2]),
                "renamed" | "durable" => assert!(!sidecar(&db, "-shm").exists()),
                "admitted" => assert!(!probe(&db).unwrap()),
                _ => unreachable!(),
            }
            let retained = fs::read_dir(db.parent().unwrap()).unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(".br-wal-index-"))
                .unwrap();
            assert!(retained.join("prepared.json").is_file());
            if stage != "prepared" {
                assert_eq!(fs::read(retained.join("poisoned-shm")).unwrap(), before[2]);
            }
            // Both the restart and a repeated recovery must preserve WAL-only
            // rows. The old per-invocation quarantine flag failed this check
            // after "renamed", "durable", and "admitted".
            recover_and_close_without_payload_changes(&db);
            recover_and_close_without_payload_changes(&db);
            assert_eq!(fs::read(&db).unwrap(), before[0]);
            assert_eq!(fs::read(sidecar(&db, "-wal")).unwrap(), before[1]);
        }
    }
}
