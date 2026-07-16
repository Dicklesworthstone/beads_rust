# wal_without_shm

- **FM**: `fm-state_files-wal-shm-sidecar-orphan` (P1) — WAL-without-SHM
  variant (benign for frankensqlite).
- **Subsystem**: state_files
- **Detect**: `db.sidecars` check stays `ok` with an "expected for
  frankensqlite" informational message — confirms the detector distinguishes
  this from the true-error SHM-without-WAL variant.
- **Repair contract**: SAFETY — `--repair` MUST NOT delete the WAL (it may
  carry committed-but-not-checkpointed data).
- **Round-trip**: N/A — no destructive auto-fix.
- **Expected exit codes**:
    - detect: 0 with the documented `RUST_LOG=error` environment (an unrelated
      verbose-log advisory may otherwise make the aggregate doctor exit 1)
    - repair: 0 or 2
    - undo: 0
