# healthy_workspace_baseline (control)

- **FM**: none (control fixture)
- **Subsystem**: meta
- **Detect**: No non-OK checks on a fresh `br init` workspace when invoked with
  the documented `RUST_LOG=error` operator environment.
- **Repair contract**: `--repair` is a no-op idempotent invocation; does NOT
  introduce new errors.
- **Round-trip**: trivial — undo has nothing to restore on a healthy
  workspace.
- **Expected exit codes**:
    - detect: 0
    - repair: 0 or 2
    - undo: 0

This is the "doctor doesn't break a clean workspace" regression — the
weakest fixture in the suite but the most important guarantee.
