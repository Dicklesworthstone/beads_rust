# mcp_serve_stale_write_lock

- **FM**: `fm-agent_coordination-mcp-serve-stale-write-lock`
- **Covered by live detector**: `write_lock` /
  `fm-concurrency_primitives-orphaned-write-lock`
- **Detect**: plants an old `.beads/.write.lock` plus an orphan
  `.write.lock.holder.pid` sidecar, matching the shape left behind by a killed
  long-running `br serve` process. Doctor must classify the lock inode `ok`
  with `details.reason == "persistent_advisory_inode"`; process exit already
  released the OS lock.
- **Repair contract**: doctor must not move, remove, or rewrite either lock
  artifact automatically. The fixture proves device+inode identity across
  detect, repair, and undo; a subsequent real mutation proves the old inode
  does not wedge the workspace.
- **Round-trip**: no chokepointed mutation is expected. `doctor undo` is a
  no-op for this fixture, and the lock artifacts remain present.
- **Expected exit codes**:
    - detect: 0
    - repair: 0
    - undo: 0 or 2

The original skeleton expected `doctor --fix --only
fm-agent_coordination-mcp-serve-stale-write-lock` to quarantine the lock.
That would be unsafe: moving the file could split future lockers onto a new
inode while an existing process still believes it owns the old one. Actual
live ownership is classified by startup lock acquisition, not file age.
