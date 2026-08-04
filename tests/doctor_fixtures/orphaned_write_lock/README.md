# orphaned_write_lock

- **FM**: `fm-concurrency_primitives-orphaned-write-lock` (P1)
- **Subsystem**: concurrency_primitives
- **Detect**: a regular `.beads/.write.lock` with an arbitrarily old mtime
  remains `ok` with `details.reason == "persistent_advisory_inode"`.
  The file is a stable lock target; ownership lives in the OS advisory lock,
  not in inode age.
- **Repair contract**: doctor never moves, removes, or rewrites the lock inode.
  Moving it while a process owns the old inode would split writers across two
  independent lock domains. The fixture records the device and inode after
  planting the lock and verifies the same identity after detect, repair, and
  undo.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 0
    - repair: 0
    - undo: 0 or 2 when no repair run exists

Live ownership is covered separately by the held-flock CLI test: flat doctor
must return the typed `concurrency_lost` startup envelope without inspecting or
mutating the workspace.
