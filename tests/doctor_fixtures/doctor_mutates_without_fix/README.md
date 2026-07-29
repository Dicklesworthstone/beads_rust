# doctor_mutates_without_fix

- **FM**: `fm-state_files-doctor-mutates-without-fix`
- **Detector contract**: read-only `br doctor --json` must not mutate `.beads/`.
- **Detect**: plants a malformed 20-byte `.beads/beads.db` while leaving the
  authoritative JSONL and a WAL sidecar in place. The old sqlite3 fallback used
  to create `.beads/beads.db-shm` while reporting the malformed database.
- **Repair contract**: normal `br doctor --repair` may rebuild the database
  from JSONL. The invariant under test is that every subsequent read-only
  `doctor --json` invocation preserves the current `.beads/` file list and
  bytes exactly.
- **Round-trip**: undo coverage is limited to the mutations recorded by the
  repair chokepoint. This fixture treats undo as best-effort and verifies only
  that the post-undo read-only doctor path remains non-mutating.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2
    - undo: 0 or 2

This promotes the original skeleton's P0 baseline into the current live
contract. The old skeleton expected `--fix --only
fm-state_files-doctor-mutates-without-fix`; the real fix is source-level
inspection hygiene, not a runtime workspace fixer.
