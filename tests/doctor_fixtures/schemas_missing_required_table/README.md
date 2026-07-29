# schemas_missing_required_table

- **FM**: `fm-schemas-missing-required-table` (P0)
- **Subsystem**: schemas
- **Detect**: `schema.tables` check goes to `error` with the message
  `Missing tables: export_hashes` and `details.missing = ["export_hashes"]`.
- **Repair contract**: `--repair` reapplies the canonical schema via the
  JSONL→DB rebuild path. `apply_schema` is additive — all DDL is
  `CREATE TABLE IF NOT EXISTS`, so existing rows in other tables are
  byte-stable.
- **Round-trip**: PARTIAL — the rebuild path's WP3/WP4 chokepoint coverage
  for DDL re-application is incomplete, so `undo latest` may report
  `restored: 0`. The fixture asserts post-undo the DB is still queryable;
  this matches the chokepoint-coverage gap documented in the suite-level
  README.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
