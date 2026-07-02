# mcp_serve_stale_write_lock

- **FM**: `fm-agent_coordination-mcp-serve-stale-write-lock`
- **Covered by live detector**: `write_lock` /
  `fm-concurrency_primitives-orphaned-write-lock`
- **Detect**: plants a stale `.beads/.write.lock` plus an orphan
  `.write.lock.holder.pid` sidecar, matching the shape left behind by a
  killed long-running `br serve` process. `br doctor --json` must warn on
  `write_lock` with `details.reason == "stale_mtime"`.
- **Repair contract**: detect-only. Doctor must not move, remove, or rewrite
  either lock artifact automatically. A stale regular lock file is not itself
  a live flock holder; once the owning process is gone, normal mutating
  commands can acquire the flock through the same path.
- **Round-trip**: no chokepointed mutation is expected. `doctor undo` is a
  no-op for this fixture, and the lock artifacts remain present.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2
    - undo: 0 or 2

The original skeleton expected `doctor --fix --only
fm-agent_coordination-mcp-serve-stale-write-lock` to quarantine the lock.
That would be unsafe: user-space stale detection cannot prove a lock file is
not held by a live writer, and moving the file could split future lockers onto
a new inode while an existing process still believes it owns the old one.
This fixture preserves the safer current behavior and adds MCP-specific
coverage for the orphan holder-pid sidecar.
