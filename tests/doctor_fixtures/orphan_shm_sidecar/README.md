# orphan_shm_sidecar

- **FM**: `fm-state_files-wal-shm-sidecar-orphan` (P1) — SHM-without-WAL variant
- **Subsystem**: state_files
- **Detect**: `db.sidecars` check goes to `error` with "SHM sidecar exists without a matching WAL sidecar"
- **Repair contract**: `--repair` quarantines the orphan SHM and rebuilds the DB from JSONL.
- **Round-trip**: PARTIAL — the JSONL→DB rebuild path predates the WP3/WP4
  `mutate()` chokepoint, so `undo latest` may report `restored: 0`. This is
  expected; the fixture exercises detect+repair only.
- **Expected exit codes**:
    - detect: 1 (findings present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0 (succeeds with no chokepointed actions to replay)
