# recovery_artifacts_orphaned

- **FM**: `fm-state_files-recovery-artifacts-orphaned` (P3)
- **Subsystem**: state_files
- **Detect**: `db.recovery_artifacts` check goes to `warn` reporting the
  preserved-artifact count
- **Repair contract**: SAFETY — no auto-prune. Recovery backups must survive
  `--repair` until the operator removes them by hand. Doctor's job is to
  surface the artifact count, not destroy potentially-irreplaceable backups.
- **Round-trip**: N/A — no auto-fix.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists)
    - undo: 0
