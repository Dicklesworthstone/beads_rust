# wal_without_shm

- **FM**: `fm-state_files-wal-shm-sidecar-orphan` (P1) — WAL-without-SHM
  variant (benign for frankensqlite).
- **Subsystem**: state_files
- **Detect**: `db.sidecars` check stays `ok` with a message that names the
  WAL — confirms the detector distinguishes this from the true-error
  SHM-without-WAL variant. Since fsqlite 0.3.15 every open, including the
  doctor's own read-only inspection, recreates `-shm`, so the message seen
  is normally "SHM sidecar ... is inert beside the WAL" rather than the
  WAL-only wording (verified 2026-09-03: `rm -shm` then `br doctor --json`
  leaves a fresh `-shm` behind).
- **Repair contract**: SAFETY — `--repair` MUST NOT delete the WAL (it may
  carry committed-but-not-checkpointed data).
- **Round-trip**: N/A — no destructive auto-fix.
- **Expected exit codes**:
    - detect: 0 with the documented `RUST_LOG=error` environment (an unrelated
      verbose-log advisory may otherwise make the aggregate doctor exit 1)
    - repair: 0 or 2
    - undo: 0
