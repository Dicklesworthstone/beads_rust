# orphaned_write_lock

- **FM**: `fm-concurrency_primitives-orphaned-write-lock` (P1)
- **Subsystem**: concurrency_primitives
- **Detect**: since GitHub #395 the `write_lock` check probes instead of
  trusting mtime. `.beads/.write.lock` is a persistent lock *target*:
  flock acquisition never updates mtime, and the kernel releases an
  advisory flock when the owning fd closes (kill -9/OOM included), so a
  leftover file wedges nothing. A stale-mtime file is only a probe
  candidate: a non-blocking `try_lock` that acquires → `ok`
  (`probe_acquired_free`); would-block → `ok` (`persistent_advisory_inode`, normal on a
  busy workspace); only an unprobeable file warns (`stale_mtime`), and
  the guidance is to investigate holders — never to move the file aside,
  because renaming it while a holder keeps the old inode locked lets the
  next writer lock a NEW inode, splitting mutual exclusion.
- **Repair contract**: SAFETY — detect-only. The doctor NEVER removes or
  renames `.write.lock` automatically.
- **This fixture**: plants an ancient-mtime FREE lock file and asserts
  the probe classifies it `ok`/`probe_acquired_free` with no move-aside
  advice, and that `--repair` leaves the file untouched.
