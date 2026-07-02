# recovery_dir_not_writable

- **FM**: `fm-permissions-recovery-dir-not-writable`
- **Detector**: `permissions.recovery_dir`
- **Detect**: creates `.beads/.br_recovery` as an owner-read/execute-only
  directory. `br doctor --json` must warn with `mode_octal == "555"` and the
  recovery-dir FM identifier in `details.finding_id`.
- **Repair contract**: detect-only. Doctor must not chmod or move the recovery
  directory because operators may intentionally lock recovery evidence for
  compliance or forensic retention.
- **Round-trip**: no repair actions are expected. The fixture restores owner
  writability after proving the invariant so the harness can run undo and clean
  up the temporary workspace.
- **Expected exit codes**:
    - detect: 1
    - repair: 0
    - undo: 0

The source skeleton expected old `--fix` behavior and an advisory artifact.
The live doctor contract is safer and narrower: it surfaces the operator action
to take and leaves the locked directory untouched.
