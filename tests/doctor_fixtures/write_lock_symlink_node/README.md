# write_lock_symlink_node

**FM:** `fm-concurrency_primitives-orphaned-write-lock` (bead `beads_rust-5sej`) — detect-only.

## Shape

`.beads/.write.lock` is a **symlink** to a sibling regular file
(`.beads/.lock_target`). Startup `OpenOptions` follows the symlink, so the
workspace still opens and doctor runs — but the advisory flock has silently
moved to the target inode, splitting mutual exclusion away from the canonical
lock path.

## Contract

- **Detect:** `write_lock` classifies the node from `lstat` alone as
  `status: error` with `reason: non_regular_lock_node`, `node_kind: symlink`.
  Doctor exits non-zero (fail closed).
- **Never mutates:** neither the symlink nor its target is removed, renamed,
  retargeted, or replaced in any stage, including `--repair` and undo. Moving
  a lock node aside while a holder has the old inode locked would split
  mutual exclusion across two files; the operator must intervene manually.

The sibling **directory** shape cannot be exercised through this harness:
a directory in the lock slot makes startup lock acquisition fail before any
check runs (which is itself fail-closed). That shape is covered by the unit
test `check_orphaned_write_lock_directory_fails_closed` and the CLI e2e in
`tests/e2e_doctor_write_lock_shapes.rs`.
