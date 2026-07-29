# gitignore_leaking_beads

- **FM**: `fm-configs-gitignore-leaking-beads`
- **Severity**: P0
- **Subsystem**: configs
- **Detect**: `gitignore.beads_inner` check goes to `warn`
- **Repair contract**: `--repair` rewrites the root `.gitignore` through the
  `mutate()` chokepoint, removing `.beads/` while preserving every other line.
- **Round-trip**: YES — chokepointed. `undo latest` restores `.gitignore`
  byte-identically; post-undo state matches `.fixture_baseline/state.tar`.
- **Expected exit codes**:
    - detect (`br doctor --json`): 1 (findings present)
    - repair (`br doctor --repair --json`): 0 or 2 (repair_applied / partial)
    - undo (`br doctor undo latest --json`): 0

This is the chokepoint reference fixture — the only one in this suite where
the full corrupt → detect → repair → undo cycle restores byte-equivalence,
because every mutation goes through `mutate()`. Other fixtures may have
chokepoint coverage gaps; see `tests/doctor_fixtures/README.md`.
