//! Receipt-authorized retention of completed, flat VACUUM backup families.
//!
//! A filename is not a completion witness. In particular, old unreceipted
//! backups and interrupted pruning directories are never adopted or removed.
//! Keep the newest five completed families and every family younger than seven
//! days. This bounds steady-state history, not bytes or uncertain evidence.
//!
//! Like recovery's no-replace exchange, removal is enabled only where flagged
//! renames are available. All namespace operations here are descriptor-relative;
//! receipts contain suffixes, never paths to follow. No recursive remover is used.

use super::{RecoveryBackupSet, db_sidecar_suffixes, verify_recovery_backup_manifest};
use crate::error::{BeadsError, Result};
use crate::sync::DatabaseFamilyWriteLock;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, mkdirat, openat,
    fstat, renameat_with, statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const SCHEMA: &str = "br.compaction.complete.v1";
const KEEP: usize = 5;
const MIN_AGE_DAYS: i64 = 7;
const RECEIPT_LIMIT: u64 = 64 * 1024;
const RECEIPT_SUFFIX: &str = ".complete.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    suffix: String,
    witness: Witness,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: String,
    database_path: String,
    stamp: String,
    completed_at: DateTime<Utc>,
    // An exact catalog also records absent sidecars and refuses future formats.
    family_suffixes: Vec<String>,
    members: Vec<Member>,
}

struct Candidate {
    receipt: Receipt,
    marker: String,
    marker_witness: Witness,
}

struct Directory {
    file: File,
    path: PathBuf,
}

fn refused(message: impl Into<String>) -> BeadsError {
    BeadsError::Config(format!("Compaction retention deferred: {}", message.into()))
}

fn syscall<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|error| BeadsError::Io(error.into()))
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn owned(metadata: &Metadata) -> bool {
    metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o022 == 0
}

fn regular(metadata: &Metadata) -> bool {
    metadata.is_file() && metadata.nlink() == 1 && owned(metadata)
}

fn safe_name(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

impl Directory {
    /// Walk each component through a retained descriptor, never through a link.
    fn open(path: &Path) -> Result<Self> {
        crate::sync::reject_symlinked_database_route_components(&path.join(".retention"))?;
        let anchor = if path.is_absolute() { "/" } else { "." };
        let mut file = File::from(syscall(openat(CWD, anchor, directory_flags(), Mode::empty()))?);
        for component in path.components() {
            match component {
                Component::Normal(name) => {
                    file = File::from(syscall(openat(&file, name, directory_flags(), Mode::empty()))?);
                }
                Component::RootDir | Component::CurDir => {}
                _ => return Err(refused("ambiguous recovery directory route")),
            }
        }
        if !owned(&file.metadata()?) {
            return Err(refused("recovery directory is not private to its owner"));
        }
        Ok(Self { file, path: path.to_path_buf() })
    }

    fn verify_route(&self) -> Result<()> {
        let current = Self::open(&self.path)?;
        if !same_inode(&current.file.metadata()?, &self.file.metadata()?) {
            return Err(refused("recovery directory identity changed"));
        }
        Ok(())
    }

    fn names(&self) -> Result<HashSet<OsString>> {
        let mut names = HashSet::new();
        for entry in syscall(Dir::read_from(&self.file))? {
            let entry = syscall(entry)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name != "." && name != ".." {
                names.insert(name.to_os_string());
            }
        }
        Ok(names)
    }

    fn open_member(&self, name: &str) -> Result<File> {
        if !safe_name(OsStr::new(name)) {
            return Err(refused("non-local recovery member name"));
        }
        // Do not even open a known alias of a live SQLite inode: closing that
        // descriptor could release this process's POSIX record locks.
        let before = syscall(statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW))?;
        if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile
            || before.st_nlink != 1
            || before.st_uid != rustix::process::geteuid().as_raw()
            || before.st_mode & 0o022 != 0
        {
            return Err(refused("non-regular, aliased, or unsafe recovery member"));
        }
        let file = File::from(syscall(openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ))?);
        let opened = syscall(fstat(&file))?;
        if before.st_dev != opened.st_dev || before.st_ino != opened.st_ino
            || !regular(&file.metadata()?) {
            return Err(refused("recovery member changed while opening"));
        }
        Ok(file)
    }

    fn write_receipt(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.verify_route()?;
        let mut file = File::from(syscall(openat(
            &self.file,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ))?);
        file.write_all(bytes)?;
        file.sync_all()?;
        self.file.sync_all()?;
        self.verify_route()
    }

    fn claim_directory(&self, marker: &str) -> Result<Self> {
        let digest = crate::util::hex_encode(&Sha256::digest(marker.as_bytes()));
        let name = format!(".compaction-pruning-{digest}");
        // An existing staging directory is evidence, not permission to resume.
        syscall(mkdirat(&self.file, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR))?;
        let file = File::from(syscall(openat(&self.file, &name, directory_flags(), Mode::empty()))?);
        if !owned(&file.metadata()?) {
            return Err(refused("pruning directory ownership changed"));
        }
        Ok(Self { file, path: self.path.join(name) })
    }

    fn move_to(&self, name: &str, target: &Self) -> Result<()> {
        syscall(renameat_with(
            &self.file, name, &target.file, name, RenameFlags::NOREPLACE,
        ))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        syscall(unlinkat(&self.file, name, AtFlags::empty()))
    }
}

fn witness(file: &mut File) -> Result<Witness> {
    let before = file.metadata()?;
    if !regular(&before) {
        return Err(refused("unsafe backup descriptor"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut reader = file.take(before.len().saturating_add(1));
    let mut length = 0_u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 { break; }
        length += u64::try_from(count).map_err(|_| refused("backup length overflow"))?;
        digest.update(&buffer[..count]);
    }
    let file = reader.into_inner();
    let after = file.metadata()?;
    if !regular(&after) || !same_inode(&before, &after)
        || length != before.len() || after.len() != before.len()
        || before.mtime() != after.mtime() || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(refused("backup changed while hashing"));
    }
    Ok(Witness {
        device: before.dev(), inode: before.ino(), length,
        modified_seconds: before.mtime(), modified_nanoseconds: before.mtime_nsec(),
        sha256: crate::util::hex_encode(&digest.finalize()),
    })
}

fn metadata_matches(metadata: &Metadata, expected: &Witness) -> bool {
    regular(metadata)
        && metadata.dev() == expected.device && metadata.ino() == expected.inode
        && metadata.len() == expected.length
        && metadata.mtime() == expected.modified_seconds
        && metadata.mtime_nsec() == expected.modified_nanoseconds
}

fn open_verified(directory: &Directory, name: &str, expected: &Witness, hash: bool) -> Result<File> {
    let mut file = directory.open_member(name)?;
    if !metadata_matches(&file.metadata()?, expected) || (hash && witness(&mut file)? != *expected) {
        return Err(refused(format!("backup witness changed: {name}")));
    }
    Ok(file)
}

fn suffixes() -> Vec<String> {
    std::iter::once("").chain(db_sidecar_suffixes().copied()).map(str::to_string).collect()
}

fn stamp_time(stamp: &str) -> Option<DateTime<Utc>> {
    let (pid, time) = stamp.strip_prefix("vacuum-")?.split_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) || pid.parse::<u32>().ok()? == 0 {
        return None;
    }
    let time = NaiveDateTime::parse_from_str(time, "%Y%m%d_%H%M%S_%f").ok()?;
    (stamp == format!("vacuum-{pid}-{}", time.format("%Y%m%d_%H%M%S_%f"))).then_some(time.and_utc())
}

fn database_name(db_path: &Path) -> Result<&str> {
    db_path.file_name().and_then(OsStr::to_str)
        .filter(|name| safe_name(OsStr::new(name)))
        .ok_or_else(|| refused("database filename cannot be represented safely"))
}

fn backup_name(database: &str, stamp: &str, suffix: &str) -> String {
    format!("{database}{suffix}.{stamp}.bak")
}

fn marker_name(database: &str, stamp: &str) -> String {
    format!("{database}.{stamp}{RECEIPT_SUFFIX}")
}

fn valid_receipt(receipt: &Receipt, db_path: &Path, marker: &str, now: DateTime<Utc>) -> bool {
    let Ok(database) = database_name(db_path) else { return false; };
    let Some(created) = stamp_time(&receipt.stamp) else { return false; };
    let known = suffixes();
    let mut seen = HashSet::new();
    receipt.schema_version == SCHEMA
        && receipt.database_path == db_path.to_string_lossy().as_ref()
        && marker == marker_name(database, &receipt.stamp)
        && created <= receipt.completed_at && receipt.completed_at <= now
        && receipt.family_suffixes == known
        && !receipt.members.is_empty() && receipt.members.len() <= known.len()
        && receipt.members.iter().all(|member| {
            known.contains(&member.suffix) && seen.insert(member.suffix.as_str())
                && member.witness.sha256.len() == 64
                && member.witness.sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        && seen.contains("")
}

/// Check the exact flat generation, including the absence of omitted sidecars.
/// Unknown sidecars or a failure/partial marker veto removal of the whole set.
fn family_names(directory: &Directory, database: &str, stamp: &str) -> Result<HashSet<OsString>> {
    let generation = format!(".{stamp}.");
    Ok(directory.names()?.into_iter().filter(|name| {
        let name = name.to_string_lossy();
        name.strip_prefix(database).is_some_and(|tail| {
            (tail.starts_with('.') || tail.starts_with('-')) && tail.contains(&generation)
        })
    }).collect())
}

fn expected_names(receipt: &Receipt, database: &str, marker: &str) -> HashSet<OsString> {
    std::iter::once(OsString::from(marker)).chain(receipt.members.iter().map(|member| {
        OsString::from(backup_name(database, &receipt.stamp, &member.suffix))
    })).collect()
}

fn load_candidate(root: &Directory, db_path: &Path, marker: &str, now: DateTime<Utc>) -> Result<Candidate> {
    let mut file = root.open_member(marker)?;
    if file.metadata()?.len() > RECEIPT_LIMIT { return Err(refused("oversized completion receipt")); }
    let marker_witness = witness(&mut file)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(RECEIPT_LIMIT + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > RECEIPT_LIMIT
        || witness(&mut file)? != marker_witness
    {
        return Err(refused("completion receipt changed while reading"));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    if !valid_receipt(&receipt, db_path, marker, now) { return Err(refused("unrecognized completion receipt")); }
    let database = database_name(db_path)?;
    if family_names(root, database, &receipt.stamp)? != expected_names(&receipt, database, marker) {
        return Err(refused("incomplete or ambiguous compaction family"));
    }
    for member in &receipt.members {
        // Protect recent generations without rehashing a week's backups on
        // every import. Hash every eligible victim again before and after claim.
        open_verified(root, &backup_name(database, &receipt.stamp, &member.suffix), &member.witness, false)?;
    }
    open_verified(root, marker, &marker_witness, true)?;
    Ok(Candidate { receipt, marker: marker.to_string(), marker_witness })
}

fn seal_completed(root: &Directory, backup_set: &RecoveryBackupSet, now: DateTime<Utc>) -> Result<String> {
    verify_recovery_backup_manifest(backup_set)?;
    let database = database_name(&backup_set.db_path)?;
    let created = stamp_time(&backup_set.stamp).ok_or_else(|| refused("unrecognized compaction stamp"))?;
    if !backup_set.had_original_database || created > now { return Err(refused("incomplete compaction manifest")); }
    let known = suffixes();
    let mut members = Vec::new();
    for ((original, backup), verified) in backup_set.files.iter().zip(&backup_set.verified_files) {
        let suffix = known.iter().find(|suffix| {
            let mut name = backup_set.db_path.as_os_str().to_os_string();
            name.push(suffix.as_str());
            Path::new(&name) == original
        }).ok_or_else(|| refused("foreign database-family member"))?;
        let name = backup_name(database, &backup_set.stamp, suffix);
        if backup != &backup_set.recovery_dir.join(&name) || verified.kind != "file" {
            return Err(refused("unrecognized backup path or artifact type"));
        }
        let mut file = root.open_member(&name)?;
        let captured = witness(&mut file)?;
        if verified.size_bytes != Some(captured.length) || verified.sha256.as_deref() != Some(captured.sha256.as_str()) {
            return Err(refused("retained backup no longer matches installation witness"));
        }
        members.push(Member { suffix: suffix.clone(), witness: captured });
    }
    let marker = marker_name(database, &backup_set.stamp);
    let receipt = Receipt {
        schema_version: SCHEMA.to_string(), database_path: backup_set.db_path.to_string_lossy().into_owned(),
        stamp: backup_set.stamp.clone(), completed_at: now, family_suffixes: known, members,
    };
    if !valid_receipt(&receipt, &backup_set.db_path, &marker, now) { return Err(refused("invalid completed family")); }
    let mut expected = expected_names(&receipt, database, &marker);
    expected.remove(OsStr::new(&marker));
    if family_names(root, database, &receipt.stamp)? != expected { return Err(refused("unexpected compaction evidence")); }
    root.write_receipt(&marker, &serde_json::to_vec(&receipt)?)?;
    Ok(marker)
}

/// Called only after candidate adoption, durable installation, reopening and
/// final database-family authority verification. Cleanup failure is non-fatal
/// to a successful compaction; the caller must not enter its rollback branch.
pub(super) fn complete_and_prune(
    backup_set: &RecoveryBackupSet,
    authority: &DatabaseFamilyWriteLock,
    now: DateTime<Utc>,
) -> Result<usize> {
    authority.verify_database_authority()?;
    let root = Directory::open(&backup_set.recovery_dir)?;
    seal_completed(&root, backup_set, now)?;
    authority.verify_database_authority()?;
    prune(&root, &backup_set.db_path, &backup_set.stamp, authority, now)
}

fn prune(root: &Directory, db_path: &Path, current: &str, authority: &DatabaseFamilyWriteLock, now: DateTime<Utc>) -> Result<usize> {
    let mut candidates = Vec::new();
    let prefix = format!("{}.vacuum-", database_name(db_path)?);
    for name in root.names()? {
        let Some(name) = name.to_str() else { continue; };
        if !name.starts_with(&prefix) || !name.ends_with(RECEIPT_SUFFIX) { continue; }
        if let Ok(candidate) = load_candidate(root, db_path, name, now) { candidates.push(candidate); }
    }
    candidates.sort_by(|left, right| {
        right.receipt.completed_at.cmp(&left.receipt.completed_at).then_with(|| right.marker.cmp(&left.marker))
    });
    let mut removed = 0;
    for candidate in candidates.into_iter().skip(KEEP) {
        if candidate.receipt.stamp == current || now - candidate.receipt.completed_at < Duration::days(MIN_AGE_DAYS) { continue; }
        authority.verify_database_authority()?;
        root.verify_route()?;
        match remove_candidate(root, db_path, &candidate, authority, |_| Ok(())) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(%error, stamp = %candidate.receipt.stamp, "Preserved uncertain compaction recovery evidence"),
        }
    }
    Ok(removed)
}

/// Claim and verify every member before the first unlink. A failed/interrupted
/// claim is deliberately not resumed: its private directory remains evidence.
fn remove_candidate(
    root: &Directory,
    db_path: &Path,
    candidate: &Candidate,
    authority: &DatabaseFamilyWriteLock,
    after_claim: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let database = database_name(db_path)?;
    let receipt = &candidate.receipt;
    let expected = expected_names(receipt, database, &candidate.marker);
    if family_names(root, database, &receipt.stamp)? != expected { return Err(refused("family changed before claim")); }
    let mut held = Vec::new();
    held.push((candidate.marker.clone(), open_verified(root, &candidate.marker, &candidate.marker_witness, true)?, &candidate.marker_witness));
    for member in &receipt.members {
        let name = backup_name(database, &receipt.stamp, &member.suffix);
        let file = open_verified(root, &name, &member.witness, true)?;
        held.push((name, file, &member.witness));
    }
    authority.verify_database_authority()?;
    root.verify_route()?;
    let claimed = root.claim_directory(&candidate.marker)?;
    for (name, _, _) in &held { root.move_to(name, &claimed)?; }
    root.file.sync_all()?;
    claimed.file.sync_all()?;
    after_claim(&claimed.path)?;
    authority.verify_database_authority()?;
    root.verify_route()?;
    claimed.verify_route()?;
    if claimed.names()? != expected || !family_names(root, database, &receipt.stamp)?.is_empty() {
        return Err(refused("family changed during claim"));
    }
    for (name, file, expected) in &mut held {
        if witness(file)? != **expected { return Err(refused("claimed descriptor changed")); }
        open_verified(&claimed, name, expected, true)?;
    }
    // The completion receipt is last. Never recursively descend into an entry,
    // and never let a pathname substitution redirect removal outside this fd.
    for (name, _, expected) in held.iter().skip(1) {
        authority.verify_database_authority()?;
        root.verify_route()?;
        claimed.verify_route()?;
        open_verified(&claimed, name, expected, false)?;
        claimed.unlink(name)?;
    }
    claimed.file.sync_all()?;
    open_verified(&claimed, &candidate.marker, &candidate.marker_witness, true)?;
    claimed.unlink(&candidate.marker)?;
    claimed.file.sync_all()?;
    root.verify_route()?;
    claimed.verify_route()?;
    let name = claimed.path.file_name().ok_or_else(|| refused("missing pruning directory name"))?;
    syscall(unlinkat(&root.file, name, AtFlags::REMOVEDIR))?;
    root.file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStorage;
    use crate::sync::blocking_database_family_write_lock_with_timeout;
    use chrono::TimeZone;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Fixture {
        storage: Option<SqliteStorage>,
        authority: Arc<DatabaseFamilyWriteLock>,
        db: PathBuf,
        root: PathBuf,
        _temp: TempDir,
    }

    fn fixture() -> Fixture {
        let temp = TempDir::new().unwrap();
        let db = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db).unwrap();
        storage.set_config("issue_prefix", "retention").unwrap();
        let authority = Arc::new(blocking_database_family_write_lock_with_timeout(
            temp.path(), &db, Some(1_000),
        ).unwrap());
        authority.bind_database_inode_for_mutation().unwrap();
        storage.attach_write_authority(Arc::clone(&authority));
        let root = temp.path().join(".br_recovery");
        fs::create_dir(&root).unwrap();
        Fixture { storage: Some(storage), authority, db, root, _temp: temp }
    }

    fn base() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).single().unwrap()
    }

    fn stamp(time: DateTime<Utc>) -> String {
        format!("vacuum-554-{}", time.format("%Y%m%d_%H%M%S_%f"))
    }

    /// Synthetic recovery bytes exercise the real receipt/claim/removal code;
    /// the integration tests below separately run the actual storage exchange.
    fn backup(db: &Path, root: &Path, time: DateTime<Utc>) -> RecoveryBackupSet {
        let stamp = stamp(time);
        let mut files = Vec::new();
        let mut verified_files = Vec::new();
        for suffix in suffixes() {
            let mut original = db.as_os_str().to_os_string();
            original.push(&suffix);
            let original = PathBuf::from(original);
            let saved = root.join(backup_name(database_name(db).unwrap(), &stamp, &suffix));
            fs::write(&saved, format!("retained {stamp} {suffix}" )).unwrap();
            let fingerprint = super::super::recovery_artifact_fingerprint(&saved).unwrap();
            verified_files.push(super::super::recovery_backup_verification(&original, &saved, &fingerprint));
            files.push((original, saved));
        }
        RecoveryBackupSet {
            db_path: db.to_path_buf(), recovery_dir: root.to_path_buf(), stamp,
            had_original_database: true, files, verified_files,
        }
    }

    fn completed(f: &Fixture, time: DateTime<Utc>) -> RecoveryBackupSet {
        let saved = backup(&f.db, &f.root, time);
        seal_completed(&Directory::open(&f.root).unwrap(), &saved, time).unwrap();
        saved
    }

    fn marker(f: &Fixture, saved: &RecoveryBackupSet) -> PathBuf {
        f.root.join(marker_name(database_name(&f.db).unwrap(), &saved.stamp))
    }

    fn protect_five(f: &Fixture) {
        for hour in 1..=5 { completed(f, base() + Duration::hours(hour)); }
    }

    fn sweep(f: &Fixture, now: DateTime<Utc>) -> usize {
        prune(&Directory::open(&f.root).unwrap(), &f.db, "not-a-candidate", &f.authority, now).unwrap()
    }

    fn assert_retained(saved: &RecoveryBackupSet) {
        for (_, path) in &saved.files { assert!(path.symlink_metadata().is_ok(), "missing {}", path.display()); }
    }

    #[test]
    fn newest_five_and_seven_day_window_are_both_protected() {
        let f = fixture();
        let old: Vec<_> = (0..10).map(|hour| completed(&f, base() + Duration::hours(hour))).collect();
        let recent: Vec<_> = (0..3).map(|hour| completed(&f, base() + Duration::days(8) + Duration::hours(hour))).collect();
        fs::write(f.root.join("operator-notes"), b"do not touch").unwrap();
        assert_eq!(sweep(&f, base() + Duration::days(9)), 8);
        for saved in &old[..8] {
            assert!(!marker(&f, saved).exists());
            for (_, path) in &saved.files { assert!(!path.exists()); }
        }
        for saved in old[8..].iter().chain(&recent) { assert_retained(saved); }
        assert_eq!(fs::read(f.root.join("operator-notes")).unwrap(), b"do not touch");
    }

    #[test]
    fn recent_high_frequency_compactions_are_not_a_hard_count_cap() {
        let f = fixture();
        let saved: Vec<_> = (0..20).map(|hour| completed(&f, base() + Duration::hours(hour))).collect();
        assert_eq!(sweep(&f, base() + Duration::days(1)), 0);
        for saved in &saved { assert_retained(saved); }
        assert_eq!(sweep(&f, base() + Duration::days(8)), 15);
    }

    #[test]
    fn exact_age_boundary_and_current_generation_are_protected() {
        let f = fixture();
        let oldest = completed(&f, base());
        protect_five(&f);
        assert_eq!(sweep(&f, base() + Duration::days(7) - Duration::nanoseconds(1)), 0);
        let root = Directory::open(&f.root).unwrap();
        assert_eq!(prune(&root, &f.db, &oldest.stamp, &f.authority, base() + Duration::days(70)).unwrap(), 0);
        assert_retained(&oldest);
        assert_eq!(sweep(&f, base() + Duration::days(7)), 1);
        assert_eq!(sweep(&f, base() + Duration::days(70)), 0);
    }

    #[test]
    fn missing_or_partial_completion_never_authorizes_legacy_backups() {
        let f = fixture();
        let legacy = backup(&f.db, &f.root, base() - Duration::days(1));
        let interrupted = backup(&f.db, &f.root, base() - Duration::days(2));
        fs::write(marker(&f, &interrupted), b"{\"schema_version\":").unwrap();
        let finished = completed(&f, base());
        protect_five(&f);
        assert_eq!(sweep(&f, base() + Duration::days(30)), 1);
        assert_retained(&legacy);
        assert_retained(&interrupted);
        assert!(!marker(&f, &finished).exists());
    }

    #[test]
    fn missing_changed_or_additional_family_members_veto_the_entire_set() {
        for damage in ["missing", "changed", "unknown-sidecar", "failure-marker"] {
            let f = fixture();
            let saved = completed(&f, base());
            protect_five(&f);
            let target = &saved.files[1].1;
            match damage {
                "missing" => fs::rename(target, f._temp.path().join("retained-outside-generation")).unwrap(),
                "changed" => fs::write(target, b"changed evidence").unwrap(),
                "unknown-sidecar" => fs::write(f.root.join(backup_name("beads.db", &saved.stamp, "-future-sidecar")), b"future evidence").unwrap(),
                _ => fs::write(f.root.join(format!("beads.db.{}.failed.json", saved.stamp)), b"failure").unwrap(),
            }
            assert_eq!(sweep(&f, base() + Duration::days(30)), 0, "{damage}");
            assert!(saved.files[0].1.exists(), "{damage}");
            assert!(marker(&f, &saved).exists(), "{damage}");
        }
    }

    #[test]
    fn same_content_replacement_inode_is_not_the_recorded_generation() {
        let f = fixture();
        let saved = completed(&f, base());
        protect_five(&f);
        let original = &saved.files[1].1;
        let bytes = fs::read(original).unwrap();
        fs::rename(original, f._temp.path().join("original-inode")).unwrap();
        fs::write(original, bytes).unwrap();
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
        assert_retained(&saved);
    }

    #[test]
    fn malformed_foreign_future_and_duplicate_proof_is_retained() {
        for damage in ["unknown-field", "foreign-db", "future", "duplicate-member", "catalog", "bad-stamp", "duplicate-field", "oversized"] {
            let f = fixture();
            let saved = completed(&f, base());
            protect_five(&f);
            let path = marker(&f, &saved);
            let original = fs::read_to_string(&path).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&original).unwrap();
            match damage {
                "unknown-field" => value["unrecognized"] = true.into(),
                "foreign-db" => value["database_path"] = "/foreign/beads.db".into(),
                "future" => value["completed_at"] = "2099-01-01T00:00:00Z".into(),
                "duplicate-member" => {
                    let member = value["members"][0].clone();
                    value["members"].as_array_mut().unwrap().push(member);
                }
                "catalog" => { value["family_suffixes"].as_array_mut().unwrap().pop(); }
                "bad-stamp" => value["stamp"] = "vacuum-554-../../outside".into(),
                _ => {}
            }
            let bytes = match damage {
                "duplicate-field" => original.replacen('{', "{\"schema_version\":\"br.compaction.complete.v1\",", 1).into_bytes(),
                "oversized" => vec![b' '; usize::try_from(RECEIPT_LIMIT).unwrap() + 1],
                _ => serde_json::to_vec(&value).unwrap(),
            };
            fs::write(path, bytes).unwrap();
            assert_eq!(sweep(&f, base() + Duration::days(30)), 0, "{damage}");
            assert_retained(&saved);
        }
    }

    #[test]
    fn different_database_families_with_identical_stamps_are_independent() {
        let f = fixture();
        let other_db = f.db.with_file_name("beads.db2");
        fs::write(&other_db, b"other live database").unwrap();
        let root = Directory::open(&f.root).unwrap();
        let mut others = Vec::new();
        for hour in 0..8 {
            completed(&f, base() + Duration::hours(hour));
            let other = backup(&other_db, &f.root, base() + Duration::hours(hour));
            seal_completed(&root, &other, base() + Duration::hours(hour)).unwrap();
            others.push(other);
        }
        assert_eq!(sweep(&f, base() + Duration::days(30)), 3);
        for other in &others { assert_retained(other); }
        assert_eq!(fs::read(&other_db).unwrap(), b"other live database");
    }

    #[test]
    fn symlinks_hardlinks_directories_and_sockets_are_never_opened_or_removed() {
        for entry in ["backup", "receipt"] {
            for kind in ["symlink", "hardlink", "directory", "socket"] {
                let f = fixture();
                let saved = completed(&f, base());
                protect_five(&f);
                let target = if entry == "backup" { saved.files[1].1.clone() } else { marker(&f, &saved) };
                let retained = f._temp.path().join("original-entry");
                fs::rename(&target, &retained).unwrap();
                let mut listener = None;
                match kind {
                    "symlink" => symlink(&retained, &target).unwrap(),
                    "hardlink" => fs::hard_link(&retained, &target).unwrap(),
                    "directory" => fs::create_dir(&target).unwrap(),
                    _ => listener = Some(UnixListener::bind(&target).unwrap()),
                }
                let before = fs::read(&retained).unwrap();
                assert_eq!(sweep(&f, base() + Duration::days(30)), 0, "{entry}/{kind}");
                assert_eq!(fs::read(&retained).unwrap(), before);
                assert!(fs::symlink_metadata(&target).is_ok());
                assert!(saved.files[0].1.exists());
                drop(listener);
            }
        }
    }

    #[test]
    fn a_replaced_recovery_directory_cannot_redirect_pruning() {
        let f = fixture();
        let saved = completed(&f, base());
        protect_five(&f);
        let root = Directory::open(&f.root).unwrap();
        let retained = f._temp.path().join("original-recovery-directory");
        fs::rename(&f.root, &retained).unwrap();
        let outside = f._temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"outside evidence").unwrap();
        symlink(&outside, &f.root).unwrap();
        assert!(prune(&root, &f.db, "none", &f.authority, base() + Duration::days(30)).is_err());
        assert!(retained.join(saved.files[0].1.file_name().unwrap()).exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"outside evidence");
    }

    #[test]
    fn changed_evidence_after_claim_is_preserved_without_any_unlink() {
        let f = fixture();
        let saved = completed(&f, base());
        let root = Directory::open(&f.root).unwrap();
        let marker = marker_name("beads.db", &saved.stamp);
        let candidate = load_candidate(&root, &f.db, &marker, base()).unwrap();
        let result = remove_candidate(&root, &f.db, &candidate, &f.authority, |claimed| {
            let member = claimed.join(saved.files[1].1.file_name().unwrap());
            let retained = claimed.join("unexpected-retained-evidence");
            fs::rename(&member, &retained)?;
            fs::write(&member, fs::read(&retained)?)?;
            Ok(())
        });
        assert!(result.is_err());
        let claimed = fs::read_dir(&f.root).unwrap().map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir()).unwrap();
        for (_, path) in &saved.files { assert!(claimed.join(path.file_name().unwrap()).exists()); }
        assert!(claimed.join(marker).exists());
        assert!(claimed.join("unexpected-retained-evidence").exists());
        protect_five(&f);
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
    }

    #[test]
    fn staging_collision_is_not_permission_to_replace_or_resume_evidence() {
        let f = fixture();
        let saved = completed(&f, base());
        let root = Directory::open(&f.root).unwrap();
        let marker = marker_name("beads.db", &saved.stamp);
        let candidate = load_candidate(&root, &f.db, &marker, base()).unwrap();
        let staged = root.claim_directory(&marker).unwrap();
        fs::write(staged.path.join("operator-evidence"), b"preserve").unwrap();
        assert!(remove_candidate(&root, &f.db, &candidate, &f.authority, |_| Ok(())).is_err());
        assert_retained(&saved);
        assert!(f.root.join(marker).exists());
        assert_eq!(fs::read(staged.path.join("operator-evidence")).unwrap(), b"preserve");
    }

    #[test]
    fn real_repeated_compactions_produce_receipts_and_age_to_five_families() {
        let mut f = fixture();
        let mut storage = f.storage.take().unwrap();
        for generation in 0..8 {
            storage.set_config("retention_generation", &generation.to_string()).unwrap();
            storage = super::super::compact_database_via_vacuum_into_in_place(storage, &f.db, Some(1_000)).unwrap();
            assert_eq!(storage.get_config("retention_generation").unwrap().as_deref(), Some(generation.to_string().as_str()));
        }
        let root = Directory::open(&f.root).unwrap();
        let markers: Vec<_> = root.names().unwrap().into_iter()
            .filter(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX)).collect();
        assert_eq!(markers.len(), 8);
        let now = Utc::now();
        for marker in &markers { load_candidate(&root, &f.db, marker.to_str().unwrap(), now).unwrap(); }
        assert_eq!(prune(&root, &f.db, "none", &f.authority, now + Duration::days(8)).unwrap(), 3);
        assert_eq!(root.names().unwrap().iter().filter(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX)).count(), 5);
        assert_eq!(storage.get_config("retention_generation").unwrap().as_deref(), Some("7"));
    }

    #[test]
    fn failed_installation_phases_never_emit_completion_receipts() {
        for phase in ["before", "adoption", "sync", "reopen"] {
            let mut f = fixture();
            let injected = || Err(refused(format!("injected {phase} failure")));
            let result = super::super::compact_database_via_vacuum_into_in_place_with_reopener(
                f.storage.take().unwrap(), &f.db, Some(1_000), &f.authority,
                |path, timeout| {
                    if phase == "reopen" { return Err(refused("injected reopen failure")); }
                    SqliteStorage::open_with_timeout_under_write_authority(path, timeout, &f.authority)
                },
                || if phase == "before" { injected() } else { Ok(()) },
                || if phase == "adoption" { injected() } else { Ok(()) },
                |from, to| if phase == "sync" { injected() } else {
                    crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io)
                },
            );
            assert!(result.is_err(), "{phase}");
            assert!(!Directory::open(&f.root).unwrap().names().unwrap().iter()
                .any(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX)), "{phase}");
            let restored = SqliteStorage::open_with_timeout_under_write_authority(&f.db, Some(1_000), &f.authority).unwrap();
            assert_eq!(restored.get_config("issue_prefix").unwrap().as_deref(), Some("retention"), "{phase}");
        }
    }

    #[test]
    fn retention_refusal_does_not_turn_successful_compaction_into_rollback() {
        let mut f = fixture();
        fs::set_permissions(&f.root, fs::Permissions::from_mode(0o777)).unwrap();
        let storage = super::super::compact_database_via_vacuum_into_in_place(
            f.storage.take().unwrap(), &f.db, Some(1_000),
        ).unwrap();
        assert_eq!(storage.get_config("issue_prefix").unwrap().as_deref(), Some("retention"));
        assert!(Directory::open(&f.root).is_err());
        assert!(fs::read_dir(&f.root).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().ends_with(".bak")));
        assert!(!fs::read_dir(&f.root).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().ends_with(RECEIPT_SUFFIX)));
    }
}
