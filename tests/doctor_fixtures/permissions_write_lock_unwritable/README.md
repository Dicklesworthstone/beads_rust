# permissions_write_lock_unwritable

- **FM**: `fm-state_files-orphaned-write-lock`
- **Detector**: `permissions.write_lock`
- **Severity**: warn
- **Repair contract**: detect-only. `br doctor --repair` must refuse before
  mutation because it cannot acquire the workspace write lock, and it must not
  chmod or remove an operator-controlled `.beads/.write.lock`.
- **Round-trip**: create a regular `.beads/.write.lock` with no owner-write
  bit -> plain `br doctor --json` emits a doctor report with
  `permissions.write_lock` -> `--repair` returns `concurrency_lost` -> undo is
  a no-op and the file remains read-only.
- **Environment skip (beads_rust-ypwu)**: the detect stage re-checks the
  planted precondition before running doctor and exits 3 (the suite's skip
  protocol) when the environment cannot hold it: the lock vanished or became
  a non-regular file, the filesystem dropped the 0444 mode bits, or the
  current uid can still write the file despite mode 444 (root or
  `CAP_DAC_OVERRIDE`, as on some remote build workers). A precondition the
  environment cannot hold is not a product failure.

