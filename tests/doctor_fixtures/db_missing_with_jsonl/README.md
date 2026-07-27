# db_missing_with_jsonl

- **FM**: `fm-state_files-empty-or-truncated-database` (P1) — the
  SQLite file family is missing entirely while `.beads/issues.jsonl`
  survives (crash mid-recovery, overzealous cleanup, partial clone).
- **Subsystem**: state_files
- **Detect**: `db.exists` reports `error` ("Missing database file")
  with the FM id in details. The read-only pass must not resurrect the
  file or touch the JSONL.
- **Repair contract**: `--repair` rebuilds the database from the
  surviving JSONL (the standard recovery-rebuild path); post-repair,
  `db.exists`/`db.open` are healthy and both seeded issues are back in
  the live store.
- **Distinct from** `empty_database_with_jsonl`, which plants a
  present-but-schema-empty DB file and exercises the schema checks;
  this fixture exercises the file-absence branch.
- **Expected exit codes**:
    - detect: 2 (error present)
    - repair: 0 (rebuild succeeds)
    - undo: 0
