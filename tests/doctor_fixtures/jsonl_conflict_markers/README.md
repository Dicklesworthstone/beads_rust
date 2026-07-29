# jsonl_conflict_markers

- **FM**: `fm-state_files-jsonl-conflict-markers` (P0)
- **Subsystem**: state_files
- **Detect**: `sync_conflict_markers` check goes to `warn`/`error`
- **Repair contract**: SAFETY GATE — `--repair` MUST NOT silently rewrite a
  file that contains git merge conflict markers. Either it refuses (exit 4
  `RefusedUnsafe`) or it skips the rebuild. The fixture asserts the markers
  survive `--repair` so we don't lose operator data.
- **Round-trip**: N/A — no destructive auto-fix to undo.
- **Expected exit codes**:
    - detect: 1 (findings present)
    - repair: 0, 2, or 4 (depending on whether refuse-gate fires); ANY exit
      that leaves the markers in place is acceptable.
    - undo: 0
