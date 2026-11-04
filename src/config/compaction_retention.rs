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
use crate::storage::SqliteStorage;
use crate::sync::DatabaseFamilyWriteLock;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use fsqlite_wal::{WAL_HEADER_SIZE, WalHeader, validate_wal_header_checksum};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, fstat, mkdirat, openat, renameat_with,
    statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

// v1 did not attest the reopened database's integrity. v2 did not attest the
// retained source's integrity. Neither can establish trustworthy recovery
// history, and neither is ever upgraded from filenames or hashes alone.
const SCHEMA: &str = "br.compaction.complete.v3";
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
    integrity_check: String,
    source_integrity_check: String,
    // An exact catalog also records absent sidecars and refuses future formats.
    family_suffixes: Vec<String>,
    members: Vec<Member>,
}

struct Candidate {
    receipt: Receipt,
    marker: String,
    marker_witness: Witness,
}

/// A clean integrity verdict bound to the exact pre-maintenance main bytes.
/// This is captured from the private source, never by reopening the live inode
/// whose POSIX locks must remain owned by the compaction authority.
#[derive(Debug, Clone)]
pub(super) struct SourceIntegrity {
    length: u64,
    sha256: String,
}

pub(super) fn capture_source_integrity(
    storage: &mut SqliteStorage,
    source_path: &Path,
) -> Result<SourceIntegrity> {
    // Opening the copied database can update its schema. Drain any such writes
    // first so the integrity probe cannot attest a WAL overlay while the proof
    // hashes an older main file. A changed snapshot will later fail the exact
    // retained-main comparison and conservatively keep all recovery evidence.
    storage.checkpoint_full()?;
    prepare_private_source_permissions(source_path)?;
    capture_source_integrity_with_check(source_path, || storage.integrity_check_messages())
}

fn capture_source_integrity_with_check(
    source_path: &Path,
    check_integrity: impl FnOnce() -> Result<Vec<String>>,
) -> Result<SourceIntegrity> {
    let parent = source_path
        .parent()
        .ok_or_else(|| refused("private compaction source has no parent"))?;
    let root = Directory::open(parent)?;
    let name = database_name(source_path)?;
    // checkpoint_full can fall back to PASSIVE without examining its counts.
    // Prove that the engine has no overlay to read instead of assuming a
    // successful checkpoint means the main file contains its complete view.
    let sidecars = snapshot_sidecars_without_frames(&root, name)?;
    let mut file = root.open_member(name)?;
    let before = witness(&mut file)?;
    require_clean_integrity(&check_integrity()?)?;
    if witness(&mut file)? != before {
        return Err(refused(
            "private source changed during integrity verification",
        ));
    }
    if snapshot_sidecars_without_frames(&root, name)? != sidecars {
        return Err(refused(
            "private source overlays changed during integrity verification",
        ));
    }
    root.verify_route()?;
    open_verified(&root, name, &before, true)?;
    Ok(SourceIntegrity {
        length: before.length,
        sha256: before.sha256,
    })
}

fn snapshot_sidecars_without_frames(
    root: &Directory,
    database: &str,
) -> Result<[Option<Witness>; 2]> {
    Ok([
        snapshot_sidecar_without_frames(root, &format!("{database}-wal"), "-wal")?,
        snapshot_sidecar_without_frames(root, &format!("{database}-journal"), "-journal")?,
    ])
}

fn snapshot_sidecar_without_frames(
    root: &Directory,
    name: &str,
    suffix: &str,
) -> Result<Option<Witness>> {
    let mut file = match root.open_member(name) {
        Ok(file) => file,
        Err(BeadsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !permitted_overlay_length(suffix, file.metadata()?.len()) {
        return Err(refused(
            "private compaction source has WAL frames or a journal",
        ));
    }
    let captured = witness(&mut file)?;
    verify_empty_overlay(&mut file, suffix, &captured)?;
    Ok(Some(captured))
}

/// A size is only a discovery filter: a header-only WAL must also parse and
/// pass the engine's checksum validation before it can establish recovery trust.
fn permitted_overlay_length(suffix: &str, length: u64) -> bool {
    match suffix {
        "-wal" => length == 0 || length == WAL_HEADER_SIZE as u64,
        "-journal" => length == 0,
        _ => true,
    }
}

fn verify_empty_overlay(file: &mut File, suffix: &str, expected: &Witness) -> Result<()> {
    if !matches!(suffix, "-wal" | "-journal") {
        return Ok(());
    }
    require_empty_overlay(file, suffix, expected.length)?;
    if witness(file)? != *expected {
        return Err(refused("WAL or journal changed while checking for frames"));
    }
    Ok(())
}

fn require_empty_overlay(file: &mut File, suffix: &str, length: u64) -> Result<()> {
    if !permitted_overlay_length(suffix, length) {
        return Err(refused("recovery family contains WAL frames or a journal"));
    }
    if suffix == "-wal" && length != 0 {
        let mut bytes = [0_u8; WAL_HEADER_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut bytes)?;
        let header = WalHeader::from_bytes(&bytes)
            .map_err(|error| refused(format!("invalid empty WAL header: {error}")))?;
        let valid = validate_wal_header_checksum(&bytes, header.big_endian_checksum())
            .map_err(|error| refused(format!("could not verify empty WAL checksum: {error}")))?;
        if !valid {
            return Err(refused("empty WAL header checksum does not match"));
        }
    }
    Ok(())
}

/// Only newly created private sources and the current in-memory installation
/// manifest may use this preparation. Historical discovery never changes modes.
struct FreshMember {
    name: String,
    file: File,
    before: Witness,
    mode: u32,
}

struct HeldMember<'a> {
    name: String,
    suffix: &'a str,
    file: File,
    expected: &'a Witness,
}

/// One discovery scan, indexed by generation. This is selection data, never
/// unlink authority: every selected keeper/victim is checked afresh below.
#[derive(Default)]
struct FamilyInventory {
    generations: HashMap<String, HashSet<OsString>>,
}

impl FamilyInventory {
    fn collect(names: &HashSet<OsString>, database: &str) -> Self {
        const DELIMITER: &[u8] = b".vacuum-";
        let mut inventory = Self::default();
        for name in names {
            let Some(tail) = name.as_bytes().strip_prefix(database.as_bytes()) else {
                continue;
            };
            if !matches!(tail.first(), Some(b'.' | b'-')) {
                continue;
            }
            // Index every generation mentioned by an ambiguous name. Using
            // only the last delimiter could hide additional recovery evidence
            // from a different generation. Preserve opaque filename bytes too.
            for (offset, window) in tail.windows(DELIMITER.len()).enumerate() {
                if window != DELIMITER {
                    continue;
                }
                let remainder = &tail[offset + 1..];
                let Some(end) = remainder.iter().position(|byte| *byte == b'.') else {
                    continue;
                };
                let Ok(stamp) = std::str::from_utf8(&remainder[..end]) else {
                    continue;
                };
                if stamp_time(stamp).is_some() {
                    inventory
                        .generations
                        .entry(stamp.to_string())
                        .or_default()
                        .insert(name.clone());
                }
            }
        }
        inventory
    }
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

fn fresh_regular(metadata: &Metadata) -> bool {
    metadata.is_file()
        && metadata.nlink() == 1
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o002 == 0
}

fn safe_name(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && Path::new(name).file_name() == Some(name)
}

impl Directory {
    /// Walk each component through a retained descriptor, never through a link.
    fn open(path: &Path) -> Result<Self> {
        crate::sync::reject_symlinked_database_route_components(&path.join(".retention"))?;
        let anchor = if path.is_absolute() { "/" } else { "." };
        let mut file = File::from(syscall(openat(
            CWD,
            anchor,
            directory_flags(),
            Mode::empty(),
        ))?);
        for component in path.components() {
            match component {
                Component::Normal(name) => {
                    file = File::from(syscall(openat(
                        &file,
                        name,
                        directory_flags(),
                        Mode::empty(),
                    ))?);
                }
                Component::RootDir | Component::CurDir => {}
                _ => return Err(refused("ambiguous recovery directory route")),
            }
        }
        if !owned(&file.metadata()?) {
            return Err(refused("recovery directory is not private to its owner"));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn verify_route(&self) -> Result<()> {
        let current = Self::open(&self.path)?;
        if !same_inode(&current.file.metadata()?, &self.file.metadata()?) {
            return Err(refused("recovery directory identity changed"));
        }
        Ok(())
    }

    fn verify_private_route(&self) -> Result<()> {
        self.verify_route()?;
        if self.file.metadata()?.mode() & 0o077 != 0 {
            return Err(refused(
                "fresh family directory is accessible to other users",
            ));
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
        if before.st_dev != opened.st_dev
            || before.st_ino != opened.st_ino
            || !regular(&file.metadata()?)
        {
            return Err(refused("recovery member changed while opening"));
        }
        Ok(file)
    }

    fn capture_fresh_member(&self, name: String, suffix: &str) -> Result<FreshMember> {
        self.verify_private_route()?;
        if !safe_name(OsStr::new(&name)) {
            return Err(refused("non-local fresh member name"));
        }
        let before = syscall(statat(&self.file, &name, AtFlags::SYMLINK_NOFOLLOW))?;
        if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile
            || before.st_nlink != 1
            || before.st_uid != rustix::process::geteuid().as_raw()
            || before.st_mode & 0o002 != 0
        {
            return Err(refused(
                "unsafe fresh member cannot have its permissions changed",
            ));
        }
        let mut file = File::from(syscall(openat(
            &self.file,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ))?);
        let opened = file.metadata()?;
        let descriptor = syscall(fstat(&file))?;
        if before.st_dev != descriptor.st_dev
            || before.st_ino != descriptor.st_ino
            || !fresh_regular(&opened)
        {
            return Err(refused("fresh member changed while opening"));
        }
        let captured = witness_with_policy(&mut file, fresh_regular)?;
        require_empty_overlay(&mut file, suffix, captured.length)?;
        if witness_with_policy(&mut file, fresh_regular)? != captured {
            return Err(refused("fresh member changed during verification"));
        }
        Ok(FreshMember {
            name,
            file,
            before: captured,
            mode: opened.mode() & 0o7777,
        })
    }

    fn make_fresh_members_private(&self, members: &mut [FreshMember]) -> Result<()> {
        // The caller validates every member and manifest before the first
        // permission change. Keep each exact descriptor through preparation.
        for member in members {
            self.verify_private_route()?;
            let named = syscall(statat(&self.file, &member.name, AtFlags::SYMLINK_NOFOLLOW))?;
            let descriptor = syscall(fstat(&member.file))?;
            if named.st_dev != descriptor.st_dev
                || named.st_ino != descriptor.st_ino
                || member.file.metadata()?.mode() & 0o7777 != member.mode
                || witness_with_policy(&mut member.file, fresh_regular)? != member.before
            {
                return Err(refused(
                    "fresh member changed before permission preparation",
                ));
            }
            // Remove access only; never grant a missing owner permission.
            let private_mode = member.mode & 0o700;
            member
                .file
                .set_permissions(std::fs::Permissions::from_mode(private_mode))?;
            member.file.sync_all()?;
            if member.file.metadata()?.mode() & 0o7777 != private_mode
                || witness(&mut member.file)? != member.before
            {
                return Err(refused(
                    "fresh member changed during permission preparation",
                ));
            }
            open_verified(self, &member.name, &member.before, true)?;
        }
        self.file.sync_all()?;
        self.verify_private_route()
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
        syscall(mkdirat(
            &self.file,
            &name,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
        ))?;
        let file = File::from(syscall(openat(
            &self.file,
            &name,
            directory_flags(),
            Mode::empty(),
        ))?);
        if !owned(&file.metadata()?) {
            return Err(refused("pruning directory ownership changed"));
        }
        Ok(Self {
            file,
            path: self.path.join(name),
        })
    }

    fn move_to(&self, name: &str, target: &Self) -> Result<()> {
        syscall(renameat_with(
            &self.file,
            name,
            &target.file,
            name,
            RenameFlags::NOREPLACE,
        ))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        syscall(unlinkat(&self.file, name, AtFlags::empty()))
    }
}

fn witness(file: &mut File) -> Result<Witness> {
    witness_with_policy(file, regular)
}

fn witness_with_policy(file: &mut File, permitted: fn(&Metadata) -> bool) -> Result<Witness> {
    let before = file.metadata()?;
    if !permitted(&before) {
        return Err(refused("unsafe backup descriptor"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut reader = file.take(before.len().saturating_add(1));
    let mut length = 0_u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        length += u64::try_from(count).map_err(|_| refused("backup length overflow"))?;
        digest.update(&buffer[..count]);
    }
    let file = reader.into_inner();
    let after = file.metadata()?;
    if !permitted(&after)
        || !same_inode(&before, &after)
        || length != before.len()
        || after.len() != before.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(refused("backup changed while hashing"));
    }
    Ok(Witness {
        device: before.dev(),
        inode: before.ino(),
        length,
        modified_seconds: before.mtime(),
        modified_nanoseconds: before.mtime_nsec(),
        sha256: crate::util::hex_encode(&digest.finalize()),
    })
}

fn prepare_private_source_permissions(source_path: &Path) -> Result<()> {
    let parent = source_path
        .parent()
        .ok_or_else(|| refused("private compaction source has no parent"))?;
    let root = Directory::open(parent)?;
    root.verify_private_route()?;
    let database = database_name(source_path)?;
    let mut members = Vec::new();
    for suffix in suffixes() {
        match root.capture_fresh_member(format!("{database}{suffix}"), &suffix) {
            Ok(member) => members.push(member),
            Err(BeadsError::Io(error))
                if !suffix.is_empty() && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    root.make_fresh_members_private(&mut members)
}

fn prepare_current_backup_permissions(
    root: &Directory,
    backup_set: &RecoveryBackupSet,
    source_integrity: &SourceIntegrity,
    now: DateTime<Utc>,
) -> Result<()> {
    verify_recovery_backup_manifest(backup_set)?;
    root.verify_private_route()?;
    let database = database_name(&backup_set.db_path)?;
    if !backup_set.had_original_database
        || stamp_time(&backup_set.stamp).is_none_or(|created| created > now)
    {
        return Err(refused("fresh family has no verified compaction manifest"));
    }
    let known = suffixes();
    let mut members = Vec::new();
    let mut names = HashSet::new();
    for ((original, backup), verified) in backup_set.files.iter().zip(&backup_set.verified_files) {
        let suffix = known
            .iter()
            .find(|suffix| {
                let mut path = backup_set.db_path.as_os_str().to_os_string();
                path.push(suffix.as_str());
                Path::new(&path) == original
            })
            .ok_or_else(|| refused("fresh manifest contains a foreign family member"))?;
        let name = backup_name(database, &backup_set.stamp, suffix);
        if backup != &backup_set.recovery_dir.join(&name)
            || verified.kind != "file"
            || !names.insert(OsString::from(&name))
        {
            return Err(refused(
                "fresh manifest has an unexpected member path or type",
            ));
        }
        let member = root.capture_fresh_member(name, suffix)?;
        if verified.size_bytes != Some(member.before.length)
            || verified.sha256.as_deref() != Some(member.before.sha256.as_str())
            || (suffix.is_empty()
                && (member.before.length != source_integrity.length
                    || member.before.sha256 != source_integrity.sha256))
        {
            return Err(refused("fresh member does not match its verified manifest"));
        }
        members.push(member);
    }
    if !names.contains(OsStr::new(&backup_name(database, &backup_set.stamp, "")))
        || family_names(root, database, &backup_set.stamp)? != names
    {
        return Err(refused(
            "fresh family is incomplete or already has recovery evidence",
        ));
    }
    root.make_fresh_members_private(&mut members)
}

fn metadata_matches(metadata: &Metadata, expected: &Witness) -> bool {
    regular(metadata)
        && metadata.dev() == expected.device
        && metadata.ino() == expected.inode
        && metadata.len() == expected.length
        && metadata.mtime() == expected.modified_seconds
        && metadata.mtime_nsec() == expected.modified_nanoseconds
}

fn open_verified(
    directory: &Directory,
    name: &str,
    expected: &Witness,
    hash: bool,
) -> Result<File> {
    let mut file = directory.open_member(name)?;
    if !metadata_matches(&file.metadata()?, expected) || (hash && witness(&mut file)? != *expected)
    {
        return Err(refused(format!("backup witness changed: {name}")));
    }
    Ok(file)
}

fn suffixes() -> Vec<String> {
    std::iter::once("")
        .chain(db_sidecar_suffixes().copied())
        .map(str::to_string)
        .collect()
}

fn stamp_time(stamp: &str) -> Option<DateTime<Utc>> {
    let (pid, time) = stamp.strip_prefix("vacuum-")?.split_once('-')?;
    let parsed_pid = pid.parse::<u32>().ok()?;
    if parsed_pid == 0 || parsed_pid.to_string() != pid {
        return None;
    }
    let time = NaiveDateTime::parse_from_str(time, "%Y%m%d_%H%M%S_%f").ok()?;
    (stamp == format!("vacuum-{pid}-{}", time.format("%Y%m%d_%H%M%S_%f"))).then_some(time.and_utc())
}

fn database_name(db_path: &Path) -> Result<&str> {
    db_path
        .file_name()
        .and_then(OsStr::to_str)
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
    let Ok(database) = database_name(db_path) else {
        return false;
    };
    let Some(created) = stamp_time(&receipt.stamp) else {
        return false;
    };
    let known = suffixes();
    let mut seen = HashSet::new();
    receipt.schema_version == SCHEMA
        && db_path.to_str() == Some(receipt.database_path.as_str())
        && receipt.integrity_check == "ok"
        && receipt.source_integrity_check == "ok"
        && marker == marker_name(database, &receipt.stamp)
        && created <= receipt.completed_at
        && receipt.completed_at <= now
        && receipt.family_suffixes == known
        && !receipt.members.is_empty()
        && receipt.members.len() <= known.len()
        && receipt.members.iter().all(|member| {
            known.contains(&member.suffix)
                && seen.insert(member.suffix.as_str())
                && permitted_overlay_length(&member.suffix, member.witness.length)
                && member.witness.sha256.len() == 64
                && member
                    .witness
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        && seen.contains("")
}

/// Check the exact flat generation, including the absence of omitted sidecars.
/// Unknown sidecars or a failure/partial marker veto removal of the whole set.
fn family_names(directory: &Directory, database: &str, stamp: &str) -> Result<HashSet<OsString>> {
    let names = directory.names()?;
    Ok(FamilyInventory::collect(&names, database)
        .generations
        .remove(stamp)
        .unwrap_or_default())
}

fn expected_names(receipt: &Receipt, database: &str, marker: &str) -> HashSet<OsString> {
    std::iter::once(OsString::from(marker))
        .chain(
            receipt.members.iter().map(|member| {
                OsString::from(backup_name(database, &receipt.stamp, &member.suffix))
            }),
        )
        .collect()
}

fn load_candidate(
    root: &Directory,
    db_path: &Path,
    marker: &str,
    now: DateTime<Utc>,
    inventory: &FamilyInventory,
) -> Result<Candidate> {
    let mut file = root.open_member(marker)?;
    if file.metadata()?.len() > RECEIPT_LIMIT {
        return Err(refused("oversized completion receipt"));
    }
    let marker_witness = witness(&mut file)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(RECEIPT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > RECEIPT_LIMIT
        || witness(&mut file)? != marker_witness
    {
        return Err(refused("completion receipt changed while reading"));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    if !valid_receipt(&receipt, db_path, marker, now) {
        return Err(refused("unrecognized completion receipt"));
    }
    let database = database_name(db_path)?;
    if inventory.generations.get(&receipt.stamp)
        != Some(&expected_names(&receipt, database, marker))
    {
        return Err(refused("incomplete or ambiguous compaction family"));
    }
    for member in &receipt.members {
        // Discovery does not grant a protected-history slot. If an old victim
        // exists, both keepers and victims require full content verification.
        open_verified(
            root,
            &backup_name(database, &receipt.stamp, &member.suffix),
            &member.witness,
            false,
        )?;
    }
    open_verified(root, marker, &marker_witness, true)?;
    Ok(Candidate {
        receipt,
        marker: marker.to_string(),
        marker_witness,
    })
}

fn seal_completed(
    root: &Directory,
    backup_set: &RecoveryBackupSet,
    source_integrity: &SourceIntegrity,
    now: DateTime<Utc>,
) -> Result<String> {
    verify_recovery_backup_manifest(backup_set)?;
    let database_path = backup_set
        .db_path
        .to_str()
        .ok_or_else(|| refused("database route cannot be represented without loss"))?;
    let database = database_name(&backup_set.db_path)?;
    let created =
        stamp_time(&backup_set.stamp).ok_or_else(|| refused("unrecognized compaction stamp"))?;
    if !backup_set.had_original_database || created > now {
        return Err(refused("incomplete compaction manifest"));
    }
    let known = suffixes();
    let mut members = Vec::new();
    for ((original, backup), verified) in backup_set.files.iter().zip(&backup_set.verified_files) {
        let suffix = known
            .iter()
            .find(|suffix| {
                let mut name = backup_set.db_path.as_os_str().to_os_string();
                name.push(suffix.as_str());
                Path::new(&name) == original
            })
            .ok_or_else(|| refused("foreign database-family member"))?;
        let name = backup_name(database, &backup_set.stamp, suffix);
        if backup != &backup_set.recovery_dir.join(&name) || verified.kind != "file" {
            return Err(refused("unrecognized backup path or artifact type"));
        }
        let mut file = root.open_member(&name)?;
        if !permitted_overlay_length(suffix, file.metadata()?.len()) {
            return Err(refused("retained family has WAL frames or a journal"));
        }
        let captured = witness(&mut file)?;
        verify_empty_overlay(&mut file, suffix, &captured)?;
        if suffix.is_empty()
            && (captured.length != source_integrity.length
                || captured.sha256 != source_integrity.sha256)
        {
            return Err(refused(
                "retained main does not match the integrity-verified source",
            ));
        }
        if verified.size_bytes != Some(captured.length)
            || verified.sha256.as_deref() != Some(captured.sha256.as_str())
        {
            return Err(refused(
                "retained backup no longer matches installation witness",
            ));
        }
        members.push(Member {
            suffix: suffix.clone(),
            witness: captured,
        });
    }
    let marker = marker_name(database, &backup_set.stamp);
    let receipt = Receipt {
        schema_version: SCHEMA.to_string(),
        database_path: database_path.to_string(),
        stamp: backup_set.stamp.clone(),
        completed_at: now,
        integrity_check: "ok".to_string(),
        source_integrity_check: "ok".to_string(),
        family_suffixes: known,
        members,
    };
    if !valid_receipt(&receipt, &backup_set.db_path, &marker, now) {
        return Err(refused("invalid completed family"));
    }
    let mut expected = expected_names(&receipt, database, &marker);
    expected.remove(OsStr::new(&marker));
    if family_names(root, database, &receipt.stamp)? != expected {
        return Err(refused("unexpected compaction evidence"));
    }
    root.write_receipt(&marker, &serde_json::to_vec(&receipt)?)?;
    Ok(marker)
}

/// Called only after candidate adoption, durable installation, reopening and
/// final database-family authority verification. Cleanup failure is non-fatal
/// to a successful compaction; the caller must not enter its rollback branch.
pub(super) fn complete_and_prune(
    backup_set: &RecoveryBackupSet,
    authority: &DatabaseFamilyWriteLock,
    reopened: &SqliteStorage,
    source_integrity: Option<&SourceIntegrity>,
    now: DateTime<Utc>,
) -> Result<usize> {
    let attached = reopened
        .attached_write_authority()
        .ok_or_else(|| refused("reopened storage has no database-family authority"))?;
    if !std::ptr::eq(attached.as_ref(), authority) {
        return Err(refused(
            "reopened storage belongs to a different write authority",
        ));
    }
    complete_and_prune_with_integrity_check(backup_set, authority, source_integrity, now, || {
        reopened.integrity_check_messages()
    })
}

fn verify_authority_for_database(
    authority: &DatabaseFamilyWriteLock,
    db_path: &Path,
) -> Result<()> {
    authority.verify_database_authority()?;
    super::reject_symlinked_database_path_for_recovery(db_path)?;
    if crate::sync::canonical_database_authority_path(db_path)?
        != authority.canonical_database_path()
    {
        return Err(refused("write authority belongs to a different database"));
    }
    authority.verify_database_authority()
}

fn complete_and_prune_with_integrity_check(
    backup_set: &RecoveryBackupSet,
    authority: &DatabaseFamilyWriteLock,
    source_integrity: Option<&SourceIntegrity>,
    now: DateTime<Utc>,
    check_integrity: impl FnOnce() -> Result<Vec<String>>,
) -> Result<usize> {
    verify_authority_for_database(authority, &backup_set.db_path)?;
    let source_integrity = source_integrity
        .ok_or_else(|| refused("pre-compaction source has no clean integrity witness"))?;
    // An engine connection opening successfully is not an integrity verdict.
    // No completion proof or pruning may follow an error, empty response, or
    // mixed diagnostic response. Failure here only defers retention.
    require_clean_integrity(&check_integrity()?)?;
    authority.verify_database_authority()?;
    let root = Directory::open(&backup_set.recovery_dir)?;
    prepare_current_backup_permissions(&root, backup_set, source_integrity, now)?;
    authority.verify_database_authority()?;
    seal_completed(&root, backup_set, source_integrity, now)?;
    authority.verify_database_authority()?;
    prune(
        &root,
        &backup_set.db_path,
        &backup_set.stamp,
        authority,
        now,
    )
}

fn require_clean_integrity(messages: &[String]) -> Result<()> {
    if matches!(messages, [message] if message.trim().eq_ignore_ascii_case("ok")) {
        return Ok(());
    }
    Err(refused("database did not pass integrity_check"))
}

fn prune(
    root: &Directory,
    db_path: &Path,
    current: &str,
    authority: &DatabaseFamilyWriteLock,
    now: DateTime<Utc>,
) -> Result<usize> {
    verify_authority_for_database(authority, db_path)?;
    root.verify_route()?;
    let mut candidates = Vec::new();
    let database = database_name(db_path)?;
    let prefix = format!("{database}.vacuum-");
    let names = root.names()?;
    let inventory = FamilyInventory::collect(&names, database);
    for name in &names {
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(RECEIPT_SUFFIX) {
            continue;
        }
        if let Ok(candidate) = load_candidate(root, db_path, name, now, &inventory) {
            candidates.push(candidate);
        }
    }
    candidates.sort_by(|left, right| {
        right
            .receipt
            .completed_at
            .cmp(&left.receipt.completed_at)
            .then_with(|| right.marker.cmp(&left.marker))
    });
    // No deletion is possible yet: avoid hashing all recent database payloads.
    if !candidates.iter().skip(KEEP).any(|candidate| {
        candidate.receipt.stamp != current
            && now - candidate.receipt.completed_at >= Duration::days(MIN_AGE_DAYS)
    }) {
        return Ok(0);
    }
    let mut removed = 0;
    let mut kept = 0;
    for candidate in &candidates {
        authority.verify_database_authority()?;
        root.verify_route()?;
        if kept < KEEP {
            match verified_members(root, db_path, candidate) {
                Ok(_) => kept += 1,
                Err(error) => tracing::warn!(%error, stamp = %candidate.receipt.stamp,
                    "Unverified compaction evidence does not displace intact retained history"),
            }
            continue;
        }
        if candidate.receipt.stamp == current
            || now - candidate.receipt.completed_at < Duration::days(MIN_AGE_DAYS)
        {
            continue;
        }
        match remove_candidate(root, db_path, candidate, authority, |_| Ok(())) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::warn!(%error, stamp = %candidate.receipt.stamp, "Preserved uncertain compaction recovery evidence")
            }
        }
    }
    Ok(removed)
}

fn verified_members<'a>(
    root: &Directory,
    db_path: &Path,
    candidate: &'a Candidate,
) -> Result<Vec<HeldMember<'a>>> {
    let database = database_name(db_path)?;
    let receipt = &candidate.receipt;
    let expected = expected_names(receipt, database, &candidate.marker);
    if family_names(root, database, &receipt.stamp)? != expected {
        return Err(refused("family changed before verification"));
    }
    let mut held = vec![HeldMember {
        name: candidate.marker.clone(),
        suffix: "",
        file: open_verified(root, &candidate.marker, &candidate.marker_witness, true)?,
        expected: &candidate.marker_witness,
    }];
    for member in &receipt.members {
        let name = backup_name(database, &receipt.stamp, &member.suffix);
        let mut file = open_verified(root, &name, &member.witness, true)?;
        verify_empty_overlay(&mut file, &member.suffix, &member.witness)?;
        held.push(HeldMember {
            name,
            suffix: &member.suffix,
            file,
            expected: &member.witness,
        });
    }
    // Recheck the marker after reading payloads: it supplies deletion authority.
    open_verified(root, &candidate.marker, &candidate.marker_witness, true)?;
    Ok(held)
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
    verify_authority_for_database(authority, db_path)?;
    let database = database_name(db_path)?;
    let receipt = &candidate.receipt;
    let expected = expected_names(receipt, database, &candidate.marker);
    let mut held = verified_members(root, db_path, candidate)?;
    authority.verify_database_authority()?;
    root.verify_route()?;
    let claimed = root.claim_directory(&candidate.marker)?;
    for member in &held {
        root.move_to(&member.name, &claimed)?;
    }
    root.file.sync_all()?;
    claimed.file.sync_all()?;
    after_claim(&claimed.path)?;
    authority.verify_database_authority()?;
    root.verify_route()?;
    claimed.verify_route()?;
    if claimed.names()? != expected || !family_names(root, database, &receipt.stamp)?.is_empty() {
        return Err(refused("family changed during claim"));
    }
    for member in &mut held {
        if witness(&mut member.file)? != *member.expected {
            return Err(refused("claimed descriptor changed"));
        }
        let mut current = open_verified(&claimed, &member.name, member.expected, true)?;
        verify_empty_overlay(&mut current, member.suffix, member.expected)?;
    }
    // The completion receipt is last. Never recursively descend into an entry,
    // and never let a pathname substitution redirect removal outside this fd.
    for member in held.iter().skip(1) {
        authority.verify_database_authority()?;
        root.verify_route()?;
        claimed.verify_route()?;
        let mut current = open_verified(&claimed, &member.name, member.expected, false)?;
        verify_empty_overlay(&mut current, member.suffix, member.expected)?;
        claimed.unlink(&member.name)?;
    }
    claimed.file.sync_all()?;
    authority.verify_database_authority()?;
    root.verify_route()?;
    claimed.verify_route()?;
    open_verified(&claimed, &candidate.marker, &candidate.marker_witness, true)?;
    claimed.unlink(&candidate.marker)?;
    claimed.file.sync_all()?;
    root.verify_route()?;
    claimed.verify_route()?;
    let name = claimed
        .path
        .file_name()
        .ok_or_else(|| refused("missing pruning directory name"))?;
    syscall(unlinkat(&root.file, name, AtFlags::REMOVEDIR))?;
    root.file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::blocking_database_family_write_lock_with_timeout;
    use chrono::TimeZone;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use tempfile::TempDir;

    // Header observed in a native, checksum-verified br 0.7.6 compaction backup.
    // A reset WAL retains this valid header but contains no frame payload.
    const OBSERVED_HEADER_ONLY_WAL: [u8; WAL_HEADER_SIZE] = [
        0x37, 0x7f, 0x06, 0x82, 0x00, 0x2d, 0xe2, 0x18, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00,
        0x05, 0xa9, 0x10, 0x44, 0x0e, 0xf0, 0x4a, 0x5f, 0x3f, 0xe8, 0x2b, 0x13, 0xbc, 0xe4, 0x72,
        0x36, 0x51,
    ];

    struct Fixture {
        storage: Option<SqliteStorage>,
        authority: Arc<DatabaseFamilyWriteLock>,
        db: PathBuf,
        root: PathBuf,
        temp: TempDir,
    }

    fn fixture() -> Fixture {
        // macOS exposes its temporary directory through /var -> /private/var.
        // Resolve the fixture's trusted parent before creating the workspace;
        // production routes must continue to reject symlinked components.
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let temp = TempDir::new_in(parent).unwrap();
        let workspace = fs::canonicalize(temp.path()).unwrap();
        let db = workspace.join("beads.db");
        let mut storage = SqliteStorage::open(&db).unwrap();
        storage.set_config("issue_prefix", "retention").unwrap();
        let authority = Arc::new(
            blocking_database_family_write_lock_with_timeout(&workspace, &db, Some(1_000)).unwrap(),
        );
        authority.bind_database_inode_for_mutation().unwrap();
        storage.attach_write_authority(Arc::clone(&authority));
        let root = workspace.join(".br_recovery");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Fixture {
            storage: Some(storage),
            authority,
            db,
            root,
            temp,
        }
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
            // Synthetic backups need the same private file mode as the real
            // database family, independent of the process umask (including 002).
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&saved)
                .unwrap();
            match suffix.as_str() {
                "-wal" => file.write_all(&OBSERVED_HEADER_ONLY_WAL).unwrap(),
                "-journal" => {}
                _ => write!(file, "retained {stamp} {suffix}").unwrap(),
            }
            let fingerprint = super::super::recovery_artifact_fingerprint(&saved).unwrap();
            verified_files.push(super::super::recovery_backup_verification(
                &original,
                &saved,
                &fingerprint,
            ));
            files.push((original, saved));
        }
        RecoveryBackupSet {
            db_path: db.to_path_buf(),
            recovery_dir: root.to_path_buf(),
            stamp,
            had_original_database: true,
            files,
            verified_files,
        }
    }

    fn completed(f: &Fixture, time: DateTime<Utc>) -> RecoveryBackupSet {
        let saved = backup(&f.db, &f.root, time);
        seal_completed(
            &Directory::open(&f.root).unwrap(),
            &saved,
            &synthetic_source_integrity(&saved),
            time,
        )
        .unwrap();
        saved
    }

    /// Policy-only fixtures use opaque bytes. Real compaction tests below
    /// separately require the actual native source-integrity capture.
    fn synthetic_source_integrity(saved: &RecoveryBackupSet) -> SourceIntegrity {
        let main = saved
            .verified_files
            .iter()
            .find(|member| member.original == saved.db_path.to_str().unwrap())
            .unwrap();
        SourceIntegrity {
            length: main.size_bytes.unwrap(),
            sha256: main.sha256.clone().unwrap(),
        }
    }

    fn write_private_file(path: &Path, bytes: &[u8]) {
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    fn socket_entry(fixture: &Fixture, target: &Path) -> UnixListener {
        // Darwin's sockaddr_un permits only a short pathname. Bind beside the
        // fixture root, then move the same socket inode to the deliberately
        // long backup/receipt name without changing the negative control.
        let short_path = fixture.temp.path().join("s");
        let listener = UnixListener::bind(&short_path).unwrap();
        fs::rename(short_path, target).unwrap();
        listener
    }

    fn marker(f: &Fixture, saved: &RecoveryBackupSet) -> PathBuf {
        f.root
            .join(marker_name(database_name(&f.db).unwrap(), &saved.stamp))
    }

    fn protect_five(f: &Fixture) {
        for hour in 1..=5 {
            completed(f, base() + Duration::hours(hour));
        }
    }

    fn sweep(f: &Fixture, now: DateTime<Utc>) -> usize {
        prune(
            &Directory::open(&f.root).unwrap(),
            &f.db,
            "not-a-candidate",
            &f.authority,
            now,
        )
        .unwrap()
    }

    fn assert_retained(saved: &RecoveryBackupSet) {
        for (_, path) in &saved.files {
            assert!(
                path.symlink_metadata().is_ok(),
                "missing {}",
                path.display()
            );
        }
    }

    fn candidate(root: &Directory, db: &Path, marker: &str, now: DateTime<Utc>) -> Candidate {
        let inventory =
            FamilyInventory::collect(&root.names().unwrap(), database_name(db).unwrap());
        load_candidate(root, db, marker, now, &inventory).unwrap()
    }

    fn corrupt_without_metadata_change(path: &Path) {
        let before = fs::metadata(path).unwrap();
        let mut bytes = fs::read(path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        let after = fs::metadata(path).unwrap();
        assert!(same_inode(&before, &after));
        assert_eq!(before.len(), after.len());
        assert_eq!(before.mtime(), after.mtime());
        assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    }

    #[test]
    fn fresh_private_sources_remove_external_access_without_adding_owner_permissions() {
        for mode in [0o644, 0o664, 0o444, 0o464] {
            let f = fixture();
            let source = f.root.join("source.db");
            let mut originals = Vec::new();
            for suffix in suffixes() {
                let path = f.root.join(format!("source.db{suffix}"));
                let bytes = match suffix.as_str() {
                    "-wal" => OBSERVED_HEADER_ONLY_WAL.as_slice(),
                    "-journal" => &[],
                    _ => b"private source evidence",
                };
                write_private_file(&path, bytes);
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                originals.push((path.clone(), fs::metadata(&path).unwrap(), bytes.to_vec()));
            }
            prepare_private_source_permissions(&source).unwrap();
            for (path, before, bytes) in originals {
                let after = fs::metadata(&path).unwrap();
                assert!(same_inode(&before, &after));
                assert_eq!(after.mode() & 0o7777, mode & 0o700);
                assert_eq!(after.mtime(), before.mtime());
                assert_eq!(after.mtime_nsec(), before.mtime_nsec());
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
            capture_source_integrity_with_check(&source, || Ok(vec!["ok".to_string()])).unwrap();
        }
    }

    #[test]
    fn fresh_permissive_backups_seal_but_legacy_permissions_are_never_repaired() {
        for mode in [0o644, 0o664] {
            let f = fixture();
            let legacy = completed(&f, base());
            for (_, path) in &legacy.files {
                fs::set_permissions(path, fs::Permissions::from_mode(0o664)).unwrap();
            }
            let mut fresh = Vec::new();
            for hour in 1..=7 {
                let time = base() + Duration::hours(hour);
                let saved = backup(&f.db, &f.root, time);
                for (_, path) in &saved.files {
                    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
                }
                assert_eq!(
                    complete_and_prune_with_integrity_check(
                        &saved,
                        &f.authority,
                        Some(&synthetic_source_integrity(&saved)),
                        time,
                        || Ok(vec!["ok".to_string()]),
                    )
                    .unwrap(),
                    0
                );
                assert!(marker(&f, &saved).exists());
                for (_, path) in &saved.files {
                    assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o600);
                }
                fresh.push(saved);
            }
            assert_eq!(sweep(&f, base() + Duration::days(30)), 2);
            assert_retained(&legacy);
            for (_, path) in &legacy.files {
                assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o664);
            }
            for saved in &fresh[2..] {
                assert_retained(saved);
            }
        }
    }

    #[test]
    fn permission_preparation_refuses_unsafe_source_entries_without_changes() {
        for suffix in ["", "-wal", "-journal"] {
            for kind in [
                "world-writable",
                "symlink",
                "hardlink",
                "directory",
                "socket",
            ] {
                let f = fixture();
                let source = f.root.join("source.db");
                let original = f.temp.path().join("original-source-entry");
                let target = f.root.join(format!("source.db{suffix}"));
                write_private_file(&source, b"source snapshot");
                fs::set_permissions(&source, fs::Permissions::from_mode(0o664)).unwrap();
                if suffix.is_empty() {
                    fs::rename(&source, &original).unwrap();
                } else {
                    write_private_file(&original, b"");
                }
                let bytes = fs::read(&original).unwrap();
                let mut listener = None;
                match kind {
                    "world-writable" => {
                        write_private_file(&target, &bytes);
                        fs::set_permissions(&target, fs::Permissions::from_mode(0o666)).unwrap();
                    }
                    "symlink" => symlink(&original, &target).unwrap(),
                    "hardlink" => fs::hard_link(&original, &target).unwrap(),
                    "directory" => fs::DirBuilder::new().mode(0o700).create(&target).unwrap(),
                    _ => listener = Some(socket_entry(&f, &target)),
                }
                let before = fs::symlink_metadata(&target).unwrap();
                assert!(
                    prepare_private_source_permissions(&source).is_err(),
                    "{suffix}/{kind}"
                );
                let after = fs::symlink_metadata(&target).unwrap();
                assert!(same_inode(&before, &after));
                assert_eq!(before.mode(), after.mode());
                assert_eq!(before.uid(), after.uid());
                assert_eq!(fs::read(&original).unwrap(), bytes);
                if !suffix.is_empty() {
                    assert_eq!(fs::metadata(&source).unwrap().mode() & 0o7777, 0o664);
                }
                drop(listener);
            }
        }
    }

    #[test]
    fn permission_preparation_never_changes_foreign_owned_files() {
        let f = fixture();
        let source = if rustix::process::geteuid().is_root() {
            let path = f.root.join("source.db");
            write_private_file(&path, b"foreign-owned evidence");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
            std::os::unix::fs::chown(&path, Some(65_534), None).unwrap();
            path
        } else {
            // An unprivileged process cannot create a foreign-owned fixture.
            // Check the same ownership predicate against an existing system
            // file, then prove preparation refuses that route without chmod.
            PathBuf::from("/usr/bin/env")
        };
        let before = fs::metadata(&source).unwrap();
        assert_ne!(before.uid(), rustix::process::geteuid().as_raw());
        assert!(!fresh_regular(&before));
        assert!(prepare_private_source_permissions(&source).is_err());
        let after = fs::metadata(&source).unwrap();
        assert!(same_inode(&before, &after));
        assert_eq!(before.mode(), after.mode());
        assert_eq!(before.uid(), after.uid());
    }

    #[test]
    fn fresh_backup_permission_changes_require_the_entire_exact_manifest() {
        for damage in [
            "payload",
            "world-writable",
            "source-proof",
            "extra",
            "receipt",
        ] {
            let f = fixture();
            let saved = backup(&f.db, &f.root, base());
            let mut source = synthetic_source_integrity(&saved);
            for (_, path) in &saved.files {
                fs::set_permissions(path, fs::Permissions::from_mode(0o664)).unwrap();
            }
            let last = &saved.files.last().unwrap().1;
            match damage {
                "payload" => fs::write(last, b"changed installation evidence").unwrap(),
                "world-writable" => {
                    fs::set_permissions(last, fs::Permissions::from_mode(0o666)).unwrap();
                }
                "source-proof" => source.length += 1,
                "extra" => {
                    write_private_file(
                        &f.root
                            .join(backup_name("beads.db", &saved.stamp, "-unknown")),
                        b"unexpected evidence",
                    );
                }
                _ => write_private_file(&marker(&f, &saved), b"existing completion evidence"),
            }
            let before: Vec<_> = saved
                .files
                .iter()
                .map(|(_, path)| (fs::metadata(path).unwrap(), fs::read(path).unwrap()))
                .collect();
            assert!(
                prepare_current_backup_permissions(
                    &Directory::open(&f.root).unwrap(),
                    &saved,
                    &source,
                    base(),
                )
                .is_err(),
                "{damage}"
            );
            for ((_, path), (before, bytes)) in saved.files.iter().zip(before) {
                let after = fs::metadata(path).unwrap();
                assert!(same_inode(&before, &after));
                assert_eq!(before.mode(), after.mode());
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn permission_preparation_requires_a_private_directory_and_unchanged_member() {
        for mutation in ["directory-mode", "replacement", "content", "hardlink"] {
            let f = fixture();
            let source = f.root.join("source.db");
            write_private_file(&source, b"source snapshot");
            fs::set_permissions(&source, fs::Permissions::from_mode(0o664)).unwrap();
            let root = Directory::open(&f.root).unwrap();
            let mut held = vec![
                root.capture_fresh_member("source.db".to_string(), "")
                    .unwrap(),
            ];
            let retained = f.temp.path().join("retained-source");
            match mutation {
                "directory-mode" => {
                    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755)).unwrap();
                }
                "replacement" => {
                    fs::rename(&source, &retained).unwrap();
                    write_private_file(&source, b"replacement source");
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o664)).unwrap();
                }
                "content" => fs::write(&source, b"changed source bytes").unwrap(),
                _ => fs::hard_link(&source, &retained).unwrap(),
            }
            assert!(
                root.make_fresh_members_private(&mut held).is_err(),
                "{mutation}"
            );
            assert_eq!(fs::metadata(&source).unwrap().mode() & 0o7777, 0o664);
            if retained.exists() {
                assert_eq!(fs::metadata(&retained).unwrap().mode() & 0o7777, 0o664);
            }
        }
    }

    #[test]
    fn corrupt_newer_backup_does_not_displace_intact_history() {
        let f = fixture();
        let saved: Vec<_> = (0..7)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        corrupt_without_metadata_change(&saved[6].files[0].1);
        // Demonstrate why metadata-only discovery cannot authorize a keeper.
        let root = Directory::open(&f.root).unwrap();
        let found = candidate(
            &root,
            &f.db,
            marker(&f, &saved[6]).file_name().unwrap().to_str().unwrap(),
            base() + Duration::days(30),
        );
        assert!(verified_members(&root, &f.db, &found).is_err());
        assert_eq!(sweep(&f, base() + Duration::days(30)), 1);
        assert!(!marker(&f, &saved[0]).exists());
        for saved in &saved[1..] {
            assert_retained(saved);
        }
    }

    #[test]
    fn fewer_than_five_intact_families_never_authorize_deletion() {
        let f = fixture();
        let saved: Vec<_> = (0..9)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        for damaged in &saved[4..] {
            corrupt_without_metadata_change(&damaged.files[0].1);
        }
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
        for saved in &saved {
            assert_retained(saved);
        }
    }

    #[test]
    fn completion_requires_one_clean_integrity_verdict() {
        for messages in [
            vec![],
            vec![""],
            vec!["corrupt"],
            vec!["ok", "corrupt"],
            vec!["ok", "ok"],
        ] {
            let messages = messages.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(require_clean_integrity(&messages).is_err());
        }
        assert!(require_clean_integrity(&["ok".to_string()]).is_ok());
        assert!(require_clean_integrity(&[" OK\n".to_string()]).is_ok());
    }

    #[test]
    fn source_integrity_accepts_absent_empty_and_valid_header_only_wal() {
        let f = fixture();
        let source = f.root.join("source.db");
        write_private_file(&source, b"source snapshot");
        let absent =
            capture_source_integrity_with_check(&source, || Ok(vec!["ok".to_string()])).unwrap();
        write_private_file(&f.root.join("source.db-wal"), b"");
        write_private_file(&f.root.join("source.db-journal"), b"");
        let empty =
            capture_source_integrity_with_check(&source, || Ok(vec!["ok".to_string()])).unwrap();
        assert_eq!(absent.length, empty.length);
        assert_eq!(absent.sha256, empty.sha256);
        let mut header = WalHeader::from_bytes(&OBSERVED_HEADER_ONLY_WAL).unwrap();
        assert_eq!(header.to_bytes().unwrap(), OBSERVED_HEADER_ONLY_WAL);
        for magic in [fsqlite_wal::WAL_MAGIC_LE, fsqlite_wal::WAL_MAGIC_BE] {
            header.magic = magic;
            fs::write(f.root.join("source.db-wal"), header.to_bytes().unwrap()).unwrap();
            let header_only =
                capture_source_integrity_with_check(&source, || Ok(vec!["ok".to_string()]))
                    .unwrap();
            assert_eq!(absent.length, header_only.length);
            assert_eq!(absent.sha256, header_only.sha256);
        }
    }

    #[test]
    fn source_integrity_refuses_malformed_or_frame_bearing_overlays_before_the_probe() {
        for suffix in ["-wal", "-journal"] {
            for length in [1, 31, 32, 33, 56, 32 + 24 + 4096] {
                let f = fixture();
                let source = f.root.join("source.db");
                write_private_file(&source, b"source snapshot");
                write_private_file(&f.root.join(format!("source.db{suffix}")), &vec![0; length]);
                assert!(
                    capture_source_integrity_with_check(&source, || panic!(
                        "an unverified overlay must refuse before the integrity probe"
                    ),)
                    .is_err(),
                    "{suffix}/{length}"
                );
            }
        }
    }

    #[test]
    fn source_integrity_requires_valid_wal_magic_version_page_size_and_checksum() {
        for damage in ["magic", "version", "page-size", "checksum", "frame-byte"] {
            let f = fixture();
            let source = f.root.join("source.db");
            write_private_file(&source, b"source snapshot");
            let mut bytes = OBSERVED_HEADER_ONLY_WAL.to_vec();
            match damage {
                "magic" => bytes[0] ^= 0x10,
                "version" => bytes[7] ^= 1,
                "page-size" => bytes[8..12].fill(0),
                "checksum" => bytes[24] ^= 1,
                _ => bytes.push(0),
            }
            if matches!(damage, "magic" | "version" | "page-size") {
                // Keep the checksum correct, so each parser guard is exercised
                // independently instead of every rejection being a bad checksum.
                fsqlite_wal::write_wal_header_checksum(&mut bytes, false).unwrap();
            }
            write_private_file(&f.root.join("source.db-wal"), &bytes);
            assert!(
                capture_source_integrity_with_check(&source, || panic!(
                    "an invalid WAL must fail before the integrity probe"
                ),)
                .is_err(),
                "{damage}"
            );
        }
    }

    #[test]
    fn source_integrity_rejects_unsafe_overlay_entries() {
        for suffix in ["-wal", "-journal"] {
            for kind in [
                "symlink",
                "hardlink",
                "directory",
                "socket",
                "group-writable",
            ] {
                let f = fixture();
                let source = f.root.join("source.db");
                let original = f.temp.path().join("retained-empty-file");
                let overlay = f.root.join(format!("source.db{suffix}"));
                write_private_file(&source, b"source snapshot");
                write_private_file(&original, b"");
                let mut listener = None;
                match kind {
                    "symlink" => symlink(&original, &overlay).unwrap(),
                    "hardlink" => fs::hard_link(&original, &overlay).unwrap(),
                    "directory" => fs::DirBuilder::new().mode(0o700).create(&overlay).unwrap(),
                    "socket" => listener = Some(socket_entry(&f, &overlay)),
                    _ => {
                        write_private_file(&overlay, b"");
                        fs::set_permissions(&overlay, fs::Permissions::from_mode(0o660)).unwrap();
                    }
                }
                assert!(
                    capture_source_integrity_with_check(&source, || panic!(
                        "unsafe overlays must refuse before the integrity probe"
                    ),)
                    .is_err(),
                    "{suffix}/{kind}"
                );
                assert!(overlay.symlink_metadata().is_ok());
                assert!(fs::read(&original).unwrap().is_empty());
                drop(listener);
            }
        }
    }

    #[test]
    fn source_integrity_rechecks_overlay_payloads_and_identities_after_the_probe() {
        for suffix in ["-wal", "-journal"] {
            for mutation in ["new-empty", "replace-empty", "payload"] {
                let f = fixture();
                let source = f.root.join("source.db");
                let overlay = f.root.join(format!("source.db{suffix}"));
                write_private_file(&source, b"source snapshot");
                if mutation != "new-empty" {
                    write_private_file(&overlay, b"");
                }
                assert!(
                    capture_source_integrity_with_check(&source, || {
                        match mutation {
                            "new-empty" => write_private_file(&overlay, b""),
                            "replace-empty" => {
                                fs::rename(&overlay, f.temp.path().join("retained-overlay"))?;
                                write_private_file(&overlay, b"");
                            }
                            _ => fs::write(&overlay, b"uncheckpointed payload")?,
                        }
                        Ok(vec!["ok".to_string()])
                    })
                    .is_err(),
                    "{suffix}/{mutation}"
                );
                assert!(overlay.exists());
            }
        }
    }

    #[test]
    fn failed_integrity_verification_neither_seals_nor_prunes() {
        for verdict in [
            Err(refused("injected integrity I/O failure")),
            Ok(vec![]),
            Ok(vec!["ok".to_string(), "corrupt".to_string()]),
        ] {
            let f = fixture();
            let previous: Vec<_> = (0..7)
                .map(|hour| completed(&f, base() + Duration::hours(hour)))
                .collect();
            let current = backup(&f.db, &f.root, base() + Duration::days(30));
            assert!(
                complete_and_prune_with_integrity_check(
                    &current,
                    &f.authority,
                    Some(&synthetic_source_integrity(&current)),
                    base() + Duration::days(30),
                    || verdict,
                )
                .is_err()
            );
            assert!(!marker(&f, &current).exists());
            for saved in previous.iter().chain(std::iter::once(&current)) {
                assert_retained(saved);
            }
        }
    }

    #[test]
    fn unverified_repair_sources_cannot_displace_the_only_trustworthy_backup() {
        let f = fixture();
        let trustworthy = completed(&f, base());
        let mut unverified = Vec::new();
        for day in 1..=8 {
            let current = backup(&f.db, &f.root, base() + Duration::days(day));
            // Even a clean reopened replacement cannot attest the older source
            // that compaction repaired. These bytes must never become keepers.
            assert!(
                complete_and_prune_with_integrity_check(
                    &current,
                    &f.authority,
                    None,
                    base() + Duration::days(day),
                    || Ok(vec!["ok".to_string()]),
                )
                .is_err()
            );
            assert!(!marker(&f, &current).exists());
            unverified.push(current);
        }
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
        assert_retained(&trustworthy);
        assert!(marker(&f, &trustworthy).exists());
        for family in &unverified {
            assert_retained(family);
        }
    }

    #[test]
    fn source_integrity_witness_must_match_the_exact_retained_main() {
        let f = fixture();
        let previous: Vec<_> = (0..7)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        let current = backup(&f.db, &f.root, base() + Duration::days(30));
        let unrelated_source = synthetic_source_integrity(&previous[0]);
        assert!(
            complete_and_prune_with_integrity_check(
                &current,
                &f.authority,
                Some(&unrelated_source),
                base() + Duration::days(30),
                || Ok(vec!["ok".to_string()]),
            )
            .is_err()
        );
        assert!(!marker(&f, &current).exists());
        for family in previous.iter().chain(std::iter::once(&current)) {
            assert_retained(family);
        }
    }

    #[test]
    fn retained_overlay_payloads_cannot_seal_or_displace_trustworthy_history() {
        for suffix in ["-wal", "-journal"] {
            let f = fixture();
            let trustworthy = completed(&f, base());
            let mut uncertain = Vec::new();
            for day in 1..=6 {
                let saved = completed(&f, base() + Duration::days(day));
                let marker_path = marker(&f, &saved);
                let mut receipt: Receipt =
                    serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
                let member = receipt
                    .members
                    .iter_mut()
                    .find(|member| member.suffix == suffix)
                    .unwrap();
                let name = backup_name("beads.db", &saved.stamp, suffix);
                fs::write(f.root.join(&name), b"unverified replay payload").unwrap();
                let root = Directory::open(&f.root).unwrap();
                member.witness = witness(&mut root.open_member(&name).unwrap()).unwrap();
                // All hashes now match the evidence, but a clean main alone
                // cannot certify a family carrying a replayable overlay.
                fs::write(&marker_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
                uncertain.push(saved);
            }
            assert_eq!(sweep(&f, base() + Duration::days(30)), 0, "{suffix}");
            assert_retained(&trustworthy);
            for saved in &uncertain {
                assert_retained(saved);
            }

            let mut current = backup(&f.db, &f.root, base() + Duration::days(30));
            let source = synthetic_source_integrity(&current);
            let mut original_name = f.db.as_os_str().to_os_string();
            original_name.push(suffix);
            let index = current
                .files
                .iter()
                .position(|(original, _)| original.as_os_str() == original_name)
                .unwrap();
            let (original, saved) = &current.files[index];
            fs::write(saved, b"unverified replay payload").unwrap();
            let fingerprint = super::super::recovery_artifact_fingerprint(saved).unwrap();
            current.verified_files[index] =
                super::super::recovery_backup_verification(original, saved, &fingerprint);
            assert!(
                complete_and_prune_with_integrity_check(
                    &current,
                    &f.authority,
                    Some(&source),
                    base() + Duration::days(30),
                    || Ok(vec!["ok".to_string()]),
                )
                .is_err()
            );
            assert!(!marker(&f, &current).exists());
            assert_retained(&current);
            assert_retained(&trustworthy);
        }
    }

    #[test]
    fn matching_receipt_hashes_do_not_authorize_malformed_header_only_wals() {
        let f = fixture();
        let trustworthy = completed(&f, base());
        let mut uncertain = Vec::new();
        for day in 1..=6 {
            let saved = completed(&f, base() + Duration::days(day));
            let path = marker(&f, &saved);
            let mut receipt: Receipt = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let member = receipt
                .members
                .iter_mut()
                .find(|member| member.suffix == "-wal")
                .unwrap();
            let name = backup_name("beads.db", &saved.stamp, "-wal");
            let mut bytes = OBSERVED_HEADER_ONLY_WAL;
            bytes[24] ^= 1;
            fs::write(f.root.join(&name), bytes).unwrap();
            let root = Directory::open(&f.root).unwrap();
            member.witness = witness(&mut root.open_member(&name).unwrap()).unwrap();
            fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            let candidate = candidate(
                &root,
                &f.db,
                path.file_name().unwrap().to_str().unwrap(),
                base() + Duration::days(30),
            );
            assert!(verified_members(&root, &f.db, &candidate).is_err());
            assert!(remove_candidate(&root, &f.db, &candidate, &f.authority, |_| Ok(())).is_err());
            uncertain.push(saved);
        }
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
        assert_retained(&trustworthy);
        assert!(marker(&f, &trustworthy).exists());
        for saved in &uncertain {
            assert_retained(saved);
        }
    }

    #[test]
    fn valid_authority_for_another_database_cannot_seal_or_prune() {
        let f = fixture();
        let other = fixture();
        let previous: Vec<_> = (0..7)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        let current = backup(&f.db, &f.root, base() + Duration::days(30));
        other.authority.verify_database_authority().unwrap();
        assert!(
            complete_and_prune_with_integrity_check(
                &current,
                &other.authority,
                Some(&synthetic_source_integrity(&current)),
                base() + Duration::days(30),
                || panic!("unrelated authority must fail before integrity verification"),
            )
            .is_err()
        );
        let root = Directory::open(&f.root).unwrap();
        assert!(
            prune(
                &root,
                &f.db,
                "none",
                &other.authority,
                base() + Duration::days(30)
            )
            .is_err()
        );
        assert!(!marker(&f, &current).exists());
        for family in previous.iter().chain(std::iter::once(&current)) {
            assert_retained(family);
        }
        other.authority.verify_database_authority().unwrap();
    }

    #[test]
    fn displaced_workspace_authority_neither_seals_nor_prunes() {
        let f = fixture();
        let previous: Vec<_> = (0..7)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        let current = backup(&f.db, &f.root, base() + Duration::days(30));
        let workspace = f.temp.path();
        fs::rename(
            workspace.join(".write.lock"),
            workspace.join("retained-write-lock"),
        )
        .unwrap();
        let other_database = workspace.join("other.db");
        let competing =
            blocking_database_family_write_lock_with_timeout(workspace, &other_database, Some(100))
                .expect("a new workspace lock demonstrates that the original guard lost authority");
        assert!(
            complete_and_prune_with_integrity_check(
                &current,
                &f.authority,
                Some(&synthetic_source_integrity(&current)),
                base() + Duration::days(30),
                || panic!("lost authority must be detected before integrity verification"),
            )
            .is_err()
        );
        assert!(!marker(&f, &current).exists());
        for saved in previous.iter().chain(std::iter::once(&current)) {
            assert_retained(saved);
        }
        drop(competing);
    }

    #[test]
    fn replaced_live_database_authority_never_prunes_history() {
        let mut f = fixture();
        let saved: Vec<_> = (0..7)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        drop(f.storage.take());
        let original = f.temp.path().join("retained-live-database");
        fs::rename(&f.db, &original).unwrap();
        fs::copy(&original, &f.db).unwrap();
        let root = Directory::open(&f.root).unwrap();
        assert!(
            prune(
                &root,
                &f.db,
                "none",
                &f.authority,
                base() + Duration::days(30)
            )
            .is_err()
        );
        for family in &saved {
            assert_retained(family);
        }
        assert!(original.exists());
        assert!(f.db.exists());
    }

    #[test]
    fn old_receipts_without_both_integrity_attestations_are_never_upgraded() {
        for schema in ["br.compaction.complete.v1", "br.compaction.complete.v2"] {
            let f = fixture();
            let saved = completed(&f, base());
            protect_five(&f);
            let path = marker(&f, &saved);
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            receipt["schema_version"] = schema.into();
            receipt
                .as_object_mut()
                .unwrap()
                .remove("source_integrity_check");
            if schema.ends_with("v1") {
                receipt.as_object_mut().unwrap().remove("integrity_check");
            }
            fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            assert_eq!(sweep(&f, base() + Duration::days(30)), 0, "{schema}");
            assert_retained(&saved);
        }
    }

    #[test]
    fn discovery_indexes_opaque_and_ambiguous_names_without_losing_evidence() {
        let first = stamp(base());
        let second = stamp(base() + Duration::hours(1));
        let ordinary = OsString::from(backup_name("beads.db", &first, ""));
        let mut opaque = OsString::from("beads.db-");
        opaque.push(OsStr::from_bytes(b"\xff"));
        opaque.push(format!(".{first}.bak"));
        let ambiguous = OsString::from(format!("beads.db.{first}.note.{second}.bak"));
        let foreign = OsString::from(backup_name("beads.db2", &first, ""));
        let names = HashSet::from([ordinary.clone(), opaque.clone(), ambiguous.clone(), foreign]);
        let inventory = FamilyInventory::collect(&names, "beads.db");
        assert_eq!(
            inventory.generations[&first],
            HashSet::from([ordinary, opaque, ambiguous.clone()])
        );
        assert_eq!(inventory.generations[&second], HashSet::from([ambiguous]));
    }

    #[test]
    fn non_utf8_database_routes_cannot_receive_lossy_receipts() {
        let f = fixture();
        let parent = f.temp.path().join(OsStr::from_bytes(b"route-\xff"));
        fs::create_dir(&parent).unwrap();
        let db = parent.join("beads.db");
        let root_path = parent.join(".br_recovery");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root_path)
            .unwrap();
        let saved = backup(&db, &root_path, base());
        let root = Directory::open(&root_path).unwrap();
        let main = &saved.verified_files[0];
        let source = SourceIntegrity {
            length: main.size_bytes.unwrap(),
            sha256: main.sha256.clone().unwrap(),
        };
        assert!(seal_completed(&root, &saved, &source, base()).is_err());
        assert_retained(&saved);
        assert!(
            !root
                .names()
                .unwrap()
                .iter()
                .any(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX))
        );
    }

    #[test]
    fn only_canonical_stamps_and_local_names_are_accepted() {
        assert!(stamp_time(&stamp(base())).is_some());
        for stamp in [
            "vacuum-0-20260901_000000_000000000",
            "vacuum-0554-20260901_000000_000000000",
            "vacuum-+554-20260901_000000_000000000",
            "vacuum-554-20260901_000000_000000",
            "vacuum-554-../../outside",
        ] {
            assert!(stamp_time(stamp).is_none(), "{stamp}");
        }
        for name in ["", ".", "..", "member/", "member/.", "../member", "/member"] {
            assert!(!safe_name(OsStr::new(name)), "{name}");
        }
        assert!(safe_name(OsStr::new("beads.db.vacuum-554.bak")));
    }

    #[test]
    fn newest_five_and_seven_day_window_are_both_protected() {
        let f = fixture();
        let old: Vec<_> = (0..10)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        let recent: Vec<_> = (0..3)
            .map(|hour| completed(&f, base() + Duration::days(8) + Duration::hours(hour)))
            .collect();
        fs::write(f.root.join("operator-notes"), b"do not touch").unwrap();
        assert_eq!(sweep(&f, base() + Duration::days(9)), 8);
        for saved in &old[..8] {
            assert!(!marker(&f, saved).exists());
            for (_, path) in &saved.files {
                assert!(!path.exists());
            }
        }
        for saved in old[8..].iter().chain(&recent) {
            assert_retained(saved);
        }
        assert_eq!(
            fs::read(f.root.join("operator-notes")).unwrap(),
            b"do not touch"
        );
    }

    #[test]
    fn recent_high_frequency_compactions_are_not_a_hard_count_cap() {
        let f = fixture();
        let saved: Vec<_> = (0..20)
            .map(|hour| completed(&f, base() + Duration::hours(hour)))
            .collect();
        assert_eq!(sweep(&f, base() + Duration::days(1)), 0);
        for saved in &saved {
            assert_retained(saved);
        }
        assert_eq!(sweep(&f, base() + Duration::days(8)), 15);
    }

    #[test]
    fn exact_age_boundary_and_current_generation_are_protected() {
        let f = fixture();
        let oldest = completed(&f, base());
        protect_five(&f);
        assert_eq!(
            sweep(&f, base() + Duration::days(7) - Duration::nanoseconds(1)),
            0
        );
        let root = Directory::open(&f.root).unwrap();
        assert_eq!(
            prune(
                &root,
                &f.db,
                &oldest.stamp,
                &f.authority,
                base() + Duration::days(70)
            )
            .unwrap(),
            0
        );
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
                "missing" => {
                    fs::rename(target, f.temp.path().join("retained-outside-generation")).unwrap()
                }
                "changed" => fs::write(target, b"changed evidence").unwrap(),
                "unknown-sidecar" => fs::write(
                    f.root
                        .join(backup_name("beads.db", &saved.stamp, "-future-sidecar")),
                    b"future evidence",
                )
                .unwrap(),
                _ => fs::write(
                    f.root.join(format!("beads.db.{}.failed.json", saved.stamp)),
                    b"failure",
                )
                .unwrap(),
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
        fs::rename(original, f.temp.path().join("original-inode")).unwrap();
        fs::write(original, bytes).unwrap();
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
        assert_retained(&saved);
    }

    #[test]
    fn malformed_foreign_future_and_duplicate_proof_is_retained() {
        for damage in [
            "unknown-field",
            "foreign-db",
            "future",
            "duplicate-member",
            "catalog",
            "bad-stamp",
            "duplicate-field",
            "oversized",
        ] {
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
                "catalog" => {
                    value["family_suffixes"].as_array_mut().unwrap().pop();
                }
                "bad-stamp" => value["stamp"] = "vacuum-554-../../outside".into(),
                _ => {}
            }
            let bytes = match damage {
                "duplicate-field" => original
                    .replacen('{', "{\"schema_version\":\"br.compaction.complete.v3\",", 1)
                    .into_bytes(),
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
            seal_completed(
                &root,
                &other,
                &synthetic_source_integrity(&other),
                base() + Duration::hours(hour),
            )
            .unwrap();
            others.push(other);
        }
        assert_eq!(sweep(&f, base() + Duration::days(30)), 3);
        for other in &others {
            assert_retained(other);
        }
        assert_eq!(fs::read(&other_db).unwrap(), b"other live database");
    }

    #[test]
    fn symlinks_hardlinks_directories_and_sockets_are_never_opened_or_removed() {
        for entry in ["backup", "receipt"] {
            for kind in ["symlink", "hardlink", "directory", "socket"] {
                let f = fixture();
                let saved = completed(&f, base());
                protect_five(&f);
                let target = if entry == "backup" {
                    saved.files[1].1.clone()
                } else {
                    marker(&f, &saved)
                };
                let retained = f.temp.path().join("original-entry");
                fs::rename(&target, &retained).unwrap();
                let mut listener = None;
                match kind {
                    "symlink" => symlink(&retained, &target).unwrap(),
                    "hardlink" => fs::hard_link(&retained, &target).unwrap(),
                    "directory" => fs::create_dir(&target).unwrap(),
                    _ => listener = Some(socket_entry(&f, &target)),
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
        let retained = f.temp.path().join("original-recovery-directory");
        fs::rename(&f.root, &retained).unwrap();
        let outside = f.temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"outside evidence").unwrap();
        symlink(&outside, &f.root).unwrap();
        assert!(
            prune(
                &root,
                &f.db,
                "none",
                &f.authority,
                base() + Duration::days(30)
            )
            .is_err()
        );
        assert!(
            retained
                .join(saved.files[0].1.file_name().unwrap())
                .exists()
        );
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"outside evidence");
    }

    #[test]
    fn canonical_fixture_paths_do_not_admit_symlinked_parent_routes() {
        let f = fixture();
        let saved = completed(&f, base());
        let alias = f.temp.path().join("workspace-alias");
        symlink(f.temp.path(), &alias).unwrap();
        let aliased_root = alias.join(".br_recovery");
        // This models macOS's /var -> /private/var temporary-directory route.
        // Only the trusted fixture setup resolves it; production admission
        // continues to reject the original route containing the symlink.
        assert!(Directory::open(&aliased_root).is_err());
        assert_eq!(fs::canonicalize(&aliased_root).unwrap(), f.root);
        assert!(Directory::open(&f.root).is_ok());
        assert_retained(&saved);
    }

    #[test]
    fn changed_evidence_after_claim_is_preserved_without_any_unlink() {
        let f = fixture();
        let saved = completed(&f, base());
        let root = Directory::open(&f.root).unwrap();
        let marker = marker_name("beads.db", &saved.stamp);
        let candidate = candidate(&root, &f.db, &marker, base());
        let result = remove_candidate(&root, &f.db, &candidate, &f.authority, |claimed| {
            let member = claimed.join(saved.files[1].1.file_name().unwrap());
            let retained = claimed.join("unexpected-retained-evidence");
            fs::rename(&member, &retained)?;
            fs::write(&member, fs::read(&retained)?)?;
            Ok(())
        });
        assert!(result.is_err());
        let claimed = fs::read_dir(&f.root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        for (_, path) in &saved.files {
            assert!(claimed.join(path.file_name().unwrap()).exists());
        }
        assert!(claimed.join(marker).exists());
        assert!(claimed.join("unexpected-retained-evidence").exists());
        protect_five(&f);
        assert_eq!(sweep(&f, base() + Duration::days(30)), 0);
    }

    #[test]
    fn lost_workspace_authority_after_claim_preserves_every_staged_member() {
        let f = fixture();
        let saved = completed(&f, base());
        let root = Directory::open(&f.root).unwrap();
        let marker = marker_name("beads.db", &saved.stamp);
        let candidate = candidate(&root, &f.db, &marker, base());
        let result = remove_candidate(&root, &f.db, &candidate, &f.authority, |_| {
            fs::rename(
                f.temp.path().join(".write.lock"),
                f.temp.path().join("retained-write-lock"),
            )?;
            Ok(())
        });
        assert!(result.is_err());
        let claimed = fs::read_dir(&f.root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        for (_, path) in &saved.files {
            assert!(claimed.join(path.file_name().unwrap()).exists());
        }
        assert!(claimed.join(marker).exists());
    }

    #[test]
    fn staging_collision_is_not_permission_to_replace_or_resume_evidence() {
        let f = fixture();
        let saved = completed(&f, base());
        let root = Directory::open(&f.root).unwrap();
        let marker = marker_name("beads.db", &saved.stamp);
        let candidate = candidate(&root, &f.db, &marker, base());
        let staged = root.claim_directory(&marker).unwrap();
        fs::write(staged.path.join("operator-evidence"), b"preserve").unwrap();
        assert!(remove_candidate(&root, &f.db, &candidate, &f.authority, |_| Ok(())).is_err());
        assert_retained(&saved);
        assert!(f.root.join(marker).exists());
        assert_eq!(
            fs::read(staged.path.join("operator-evidence")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn real_repeated_compactions_produce_receipts_and_age_to_five_families() {
        let mut f = fixture();
        let mut storage = f.storage.take().unwrap();
        for generation in 0..8 {
            storage
                .set_config("retention_generation", &generation.to_string())
                .unwrap();
            storage = super::super::compact_database_via_vacuum_into_in_place(
                storage,
                &f.db,
                Some(1_000),
            )
            .unwrap();
            assert_eq!(
                storage
                    .get_config("retention_generation")
                    .unwrap()
                    .as_deref(),
                Some(generation.to_string().as_str())
            );
        }
        let root = Directory::open(&f.root).unwrap();
        let markers: Vec<_> = root
            .names()
            .unwrap()
            .into_iter()
            .filter(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX))
            .collect();
        assert_eq!(markers.len(), 8);
        let now = Utc::now();
        for marker in &markers {
            candidate(&root, &f.db, marker.to_str().unwrap(), now);
        }
        assert_eq!(
            prune(&root, &f.db, "none", &f.authority, now + Duration::days(8)).unwrap(),
            3
        );
        assert_eq!(
            root.names()
                .unwrap()
                .iter()
                .filter(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX))
                .count(),
            5
        );
        assert_eq!(
            storage
                .get_config("retention_generation")
                .unwrap()
                .as_deref(),
            Some("7")
        );
    }

    #[test]
    fn failed_installation_phases_never_emit_completion_receipts() {
        for phase in ["before", "adoption", "sync", "reopen"] {
            let mut f = fixture();
            let injected = || Err(refused(format!("injected {phase} failure")));
            let result = super::super::compact_database_via_vacuum_into_in_place_with_reopener(
                f.storage.take().unwrap(),
                &f.db,
                Some(1_000),
                &f.authority,
                |path, timeout| {
                    if phase == "reopen" {
                        return Err(refused("injected reopen failure"));
                    }
                    SqliteStorage::open_with_timeout_under_write_authority(
                        path,
                        timeout,
                        &f.authority,
                    )
                },
                || {
                    if phase == "before" {
                        injected()
                    } else {
                        Ok(())
                    }
                },
                || {
                    if phase == "adoption" {
                        injected()
                    } else {
                        Ok(())
                    }
                },
                |from, to| {
                    if phase == "sync" {
                        injected()
                    } else {
                        crate::util::sync_rename_parent_directories(from, to)
                            .map_err(BeadsError::Io)
                    }
                },
            );
            assert!(result.is_err(), "{phase}");
            assert!(
                !Directory::open(&f.root)
                    .unwrap()
                    .names()
                    .unwrap()
                    .iter()
                    .any(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX)),
                "{phase}"
            );
            let restored = SqliteStorage::open_with_timeout_under_write_authority(
                &f.db,
                Some(1_000),
                &f.authority,
            )
            .unwrap();
            assert_eq!(
                restored.get_config("issue_prefix").unwrap().as_deref(),
                Some("retention"),
                "{phase}"
            );
        }
    }

    #[test]
    fn retention_refusal_does_not_turn_successful_compaction_into_rollback() {
        let mut f = fixture();
        let before = fs::metadata(&f.db).unwrap();
        let storage = super::super::compact_database_via_vacuum_into_in_place_with_reopener(
            f.storage.take().unwrap(),
            &f.db,
            Some(1_000),
            &f.authority,
            |path, timeout| {
                let reopened = SqliteStorage::open_with_timeout_under_write_authority(
                    path,
                    timeout,
                    &f.authority,
                )?;
                // Inject refusal after opening; do not accidentally fail the
                // engine's private-source admission before retention is reached.
                fs::set_permissions(&f.root, fs::Permissions::from_mode(0o777))?;
                Ok(reopened)
            },
            || Ok(()),
            || Ok(()),
            |from, to| {
                crate::util::sync_rename_parent_directories(from, to).map_err(BeadsError::Io)
            },
        )
        .unwrap();
        assert!(!same_inode(&before, &fs::metadata(&f.db).unwrap()));
        assert_eq!(
            storage.get_config("issue_prefix").unwrap().as_deref(),
            Some("retention")
        );
        assert!(Directory::open(&f.root).is_err());
        assert!(fs::read_dir(&f.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".bak")
        }));
        assert!(!fs::read_dir(&f.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(RECEIPT_SUFFIX)
        }));
    }

    #[test]
    fn successful_real_compaction_automatically_prunes_expired_families() {
        let mut f = fixture();
        let start = Utc::now() - Duration::days(10);
        let previous: Vec<_> = (0..7)
            .map(|hour| completed(&f, start + Duration::hours(hour)))
            .collect();
        let unfinished = backup(&f.db, &f.root, start - Duration::days(1));
        let storage = super::super::compact_database_via_vacuum_into_in_place(
            f.storage.take().unwrap(),
            &f.db,
            Some(1_000),
        )
        .unwrap();
        // Seven old completed generations plus the just-completed generation:
        // exactly the three oldest are removed by the real success hook.
        for saved in &previous[..3] {
            assert!(!marker(&f, saved).exists());
            for (_, path) in &saved.files {
                assert!(!path.exists());
            }
        }
        for saved in previous[3..].iter().chain(std::iter::once(&unfinished)) {
            assert_retained(saved);
        }
        let root = Directory::open(&f.root).unwrap();
        assert_eq!(
            root.names()
                .unwrap()
                .iter()
                .filter(|name| name.to_string_lossy().ends_with(RECEIPT_SUFFIX))
                .count(),
            KEEP
        );
        assert_eq!(
            storage.get_config("issue_prefix").unwrap().as_deref(),
            Some("retention")
        );
    }
}
