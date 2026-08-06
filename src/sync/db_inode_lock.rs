//! SQLite-compatible exclusive advisory locking for the beads database inode.
//!
//! # Why this module exists (GitHub #412)
//!
//! The database-family write authority must hold an exclusive advisory lock on
//! the database *inode itself* so hard-link aliases of one physical database
//! cannot acquire two independent authorities. v0.2.20 implemented that lock
//! with [`std::fs::File::try_lock`], which is `flock(LOCK_EX)` on Unix and a
//! whole-file `LockFileEx` on Windows. That is safe for dedicated lock
//! sidecars (`.write.lock`, `.br-db-write-*.lock`) but catastrophically wrong
//! for the SQLite database file, because the SQLite engine (frankensqlite)
//! places its own advisory locks on that same inode:
//!
//! - **macOS / BSD:** `flock` locks and POSIX `fcntl` record locks live in one
//!   kernel lock table and conflict with each other — *even inside a single
//!   process*, because a `flock` is owned by its open file description while a
//!   POSIX record lock is owned by the process. Holding `flock(LOCK_EX)` on
//!   the database made every subsequent frankensqlite `fcntl(F_SETLK)` fail
//!   with `EAGAIN`, surfacing as "Database error: database is busy" on a
//!   freshly initialised workspace.
//! - **Windows:** `LockFileEx` locks are mandatory. A whole-file exclusive
//!   lock both collides with the engine's own lock-byte ranges and blocks
//!   plain reads through any other handle, so even the schema header became
//!   unreadable ("database schema is missing or unreadable").
//! - **Linux:** `flock` and POSIX record locks never interact, which is the
//!   only reason the defect stayed invisible on the primary development
//!   platform.
//!
//! # The fix
//!
//! Lock exactly **one byte at an offset the SQLite engine never locks and no
//! database ever stores data at** ([`DATABASE_INODE_LOCK_OFFSET`]), using a
//! lock family that composes with the engine on every supported platform:
//!
//! - **Unix (Linux, Android, macOS, iOS):** an open-file-description lock
//!   (`fcntl(F_OFD_SETLK)`), which conflicts range-wise with other OFD and
//!   POSIX locks — giving cross-process (and cross-authority, same-process)
//!   exclusion — but never overlaps SQLite's lock-byte range
//!   (`0x4000_0000..0x4000_0200`). Unlike classic `F_SETLK` process locks,
//!   OFD locks are *not* dropped when the process closes an unrelated file
//!   descriptor for the same inode (the engine opens and closes connections
//!   freely while the authority is held), and two authorities in one process
//!   still conflict because each has its own open file description.
//! - **Windows:** a one-byte `LockFileEx` at the same offset. The offset is
//!   far beyond any real database size, so the mandatory-lock semantics can
//!   never block the engine's I/O, and the engine's own lock ranges never
//!   overlap it.
//! - **Other Unix (unsupported release targets):** falls back to
//!   `File::try_lock` (`flock`), preserving pre-existing behavior.
//!
//! The lock is released when the `File` (its open file description / handle)
//! is closed, exactly like the previous `try_lock` semantics, so the
//! surrounding authority state machine (`retired_locks`, rebind, restore) is
//! unchanged.
//!
//! This module is the crate's single sanctioned `unsafe` exemption: the two
//! syscalls (`fcntl` / `LockFileEx`) have no safe wrapper covering OFD locks
//! on macOS (`nix` gates `F_OFD_SETLK` to linux/android) or byte-range locks
//! on Windows. Everything else in the crate remains `#![deny(unsafe_code)]`.

use std::fs::{File, TryLockError};

/// Byte offset of the beads database-inode authority lock.
///
/// Chosen so it can never collide with anything else that locks or reads the
/// database file:
///
/// - SQLite engines lock bytes in `0x4000_0000..0x4000_0200` (pending,
///   reserved, shared range). This offset is `i64::MAX - 1`, astronomically
///   beyond that range.
/// - No real database file can reach this offset (SQLite's maximum database
///   size is far smaller), so on Windows the mandatory byte lock can never
///   intersect actual data I/O.
pub(crate) const DATABASE_INODE_LOCK_OFFSET: i64 = i64::MAX - 1;

/// Try to acquire the exclusive database-inode authority lock, non-blocking.
///
/// Returns `Err(TryLockError::WouldBlock)` when another open file description
/// (any process, or another authority in this process) holds the byte, and
/// `Err(TryLockError::Error(_))` for real I/O failures. The lock is released
/// when `file` is closed.
///
/// Re-invoking on the same `File` while the byte is already held by that same
/// open file description succeeds (the lock is re-granted), matching
/// `File::try_lock` retry-loop semantics.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
#[allow(unsafe_code)]
pub(crate) fn try_lock_database_inode(file: &File) -> Result<(), TryLockError> {
    use std::os::fd::AsRawFd;

    let lock = libc::flock {
        l_type: libc::c_short::try_from(libc::F_WRLCK).unwrap_or(1),
        l_whence: libc::c_short::try_from(libc::SEEK_SET).unwrap_or(0),
        l_start: libc::off_t::try_from(DATABASE_INODE_LOCK_OFFSET).unwrap_or(libc::off_t::MAX - 1),
        l_len: 1,
        // Required to be zero for open-file-description locks.
        l_pid: 0,
    };
    // SAFETY: `fcntl(fd, F_OFD_SETLK, &flock)` only reads the `flock` struct,
    // which outlives the call, and `fd` is a valid open descriptor borrowed
    // from `file` for the duration of the call. No memory is handed to the
    // kernel beyond the struct pointer, and no Rust invariants depend on the
    // advisory lock state.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // POSIX allows either EAGAIN or EACCES for a held conflicting lock.
        Some(code) if code == libc::EAGAIN || code == libc::EACCES => Err(TryLockError::WouldBlock),
        _ => Err(TryLockError::Error(error)),
    }
}

/// Fallback for Unix platforms outside the supported release matrix: keep the
/// historical whole-file `flock`. These platforms may exhibit the BSD
/// flock/fcntl lock-table interaction, but none of them are shipped targets,
/// and silently taking no lock at all would be strictly worse.
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
#[allow(clippy::incompatible_msrv)]
pub(crate) fn try_lock_database_inode(file: &File) -> Result<(), TryLockError> {
    file.try_lock()
}

/// Windows implementation: one-byte `LockFileEx` at
/// [`DATABASE_INODE_LOCK_OFFSET`].
#[cfg(windows)]
#[allow(unsafe_code, non_snake_case, clippy::items_after_statements)]
pub(crate) fn try_lock_database_inode(file: &File) -> Result<(), TryLockError> {
    use std::os::windows::io::AsRawHandle;

    // Minimal local declarations mirroring `windows-sys`; kept private so the
    // unsafe surface stays inside this module.
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: isize,
    }

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            hFile: isize,
            dwFlags: u32,
            dwReserved: u32,
            nNumberOfBytesToLockLow: u32,
            nNumberOfBytesToLockHigh: u32,
            lpOverlapped: *mut Overlapped,
        ) -> i32;
    }

    #[allow(clippy::cast_sign_loss)]
    let offset = DATABASE_INODE_LOCK_OFFSET as u64;
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: (offset & 0xFFFF_FFFF) as u32,
        offset_high: (offset >> 32) as u32,
        h_event: 0,
    };
    // SAFETY: the handle is valid for the duration of the call (borrowed from
    // `file`), `overlapped` outlives the synchronous call, and with
    // LOCKFILE_FAIL_IMMEDIATELY on a synchronous handle the call completes
    // before returning, so the kernel keeps no reference to `overlapped`.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as isize,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(ERROR_LOCK_VIOLATION) => Err(TryLockError::WouldBlock),
        _ => Err(TryLockError::Error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(dir: &std::path::Path) -> File {
        let path = dir.join("inode-lock-test.db");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .expect("open temp lock file");
        file.write_all(b"x").expect("seed file");
        file
    }

    #[test]
    fn lock_is_exclusive_across_open_file_descriptions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = temp_file(dir.path());
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("inode-lock-test.db"))
            .expect("second open");

        try_lock_database_inode(&first).expect("first lock must succeed");
        assert!(
            matches!(
                try_lock_database_inode(&second),
                Err(TryLockError::WouldBlock)
            ),
            "second open file description must observe the held lock"
        );

        drop(first);
        try_lock_database_inode(&second).expect("lock must be acquirable after the holder closes");
    }

    #[test]
    fn relock_on_same_file_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = temp_file(dir.path());
        try_lock_database_inode(&file).expect("initial lock");
        try_lock_database_inode(&file).expect("re-lock on the same open file description");
    }

    /// The database-inode authority must not conflict with SQLite-engine
    /// style POSIX record locks on the engine's lock-byte range — the exact
    /// regression behind GitHub #412 (fatal on macOS/BSD lock tables, where
    /// the former whole-file `flock` collided with `fcntl` record locks).
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    #[test]
    #[allow(unsafe_code)]
    fn coexists_with_sqlite_engine_record_locks() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().expect("tempdir");
        let authority = temp_file(dir.path());
        try_lock_database_inode(&authority).expect("authority lock");

        // Simulate frankensqlite taking a write lock on the SQLite
        // pending-byte range through a second open file description.
        let engine = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("inode-lock-test.db"))
            .expect("engine open");
        let lock = libc::flock {
            l_type: libc::c_short::try_from(libc::F_WRLCK).unwrap_or(1),
            l_whence: libc::c_short::try_from(libc::SEEK_SET).unwrap_or(0),
            l_start: 0x4000_0000,
            l_len: 2,
            l_pid: 0,
        };
        // SAFETY: same contract as `try_lock_database_inode` — the struct and
        // descriptor are valid for the duration of the call.
        let rc = unsafe { libc::fcntl(engine.as_raw_fd(), libc::F_OFD_SETLK, &lock) };
        assert_eq!(
            rc,
            0,
            "engine-range record lock must not observe the inode authority: {}",
            std::io::Error::last_os_error()
        );
    }
}
