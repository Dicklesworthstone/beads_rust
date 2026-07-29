# empty_database_with_jsonl

- **FM**: `fm-state_files-empty-or-truncated-database` (P0)
- **Subsystem**: state_files
- **Detect**: `schema.tables` check goes to `error` (no tables in zero-byte DB);
  `workspace_health = recoverable`
- **Repair contract**: `--repair` rebuilds the DB from `.beads/issues.jsonl`,
  preserving issue history. Quarantines the truncated DB under
  `.beads/.br_recovery/`.
- **Round-trip**: PARTIAL — DB rebuild predates WP3/WP4 chokepoint. `undo
  latest` may report `restored: 0`.
- **Expected exit codes**:
    - detect: 1 (errors present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
