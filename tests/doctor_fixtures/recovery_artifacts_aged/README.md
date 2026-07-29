# recovery_artifacts_aged

- **FM**: `fm-state_files-recovery-artifacts-orphaned` (P3) — aged-TTL variant
- **Subsystem**: state_files
- **Detect**: `db.recovery_artifacts.aged` check goes to `warn` when one or
  more recovery artifacts under `.beads/.br_recovery/` (or sibling
  `.bad_<TS>` files next to the live DB) have `mtime` older than 30 days
  (the `RECOVERY_AGED_TTL_DAYS` constant in `src/cli/commands/doctor.rs`).
  The existing `db.recovery_artifacts` info-only check ALSO fires
  enumerating ALL preserved artifacts — these are intentionally
  complementary surfaces.
- **Repair contract**: SAFETY — `--repair` renames each aged artifact
  into `<run-dir>/quarantine/.beads/.br_recovery/` via the `mutate()`
  chokepoint. **Recent artifacts (younger than the TTL) are preserved
  in place** because operators commonly need recent recovery backups
  for forensic value. Per AGENTS.md RULE 1: rename, never delete.
- **Round-trip**: plant a recent + an aged artifact → detect warn on
  `db.recovery_artifacts.aged` → `--repair` quarantines only the aged
  artifact → recent artifact still present → re-detect ok on the aged
  check → `doctor undo latest` restores the aged artifact.
- **Idempotence**: a second `--repair` finds no aged artifacts and is a
  no-op (zero rename actions emitted).
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (aged quarantined)
    - undo: 0 (aged restored)
