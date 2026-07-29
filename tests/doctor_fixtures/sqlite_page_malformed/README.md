# sqlite_page_malformed

- **FM**: `fm-state_files-sqlite-page-malformed` (P0)
- **Subsystem**: state_files
- **Detect**: `sqlite.integrity_check` and/or `sqlite3.integrity_check` go to
  `error` reporting "database disk image is malformed"
- **Repair contract**: `--repair` rebuilds the DB from `issues.jsonl`, which
  bypasses the corrupted pages.
- **Round-trip**: PARTIAL — DB rebuild predates the chokepoint. `undo latest`
  may report `restored: 0`.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2
    - undo: 0
