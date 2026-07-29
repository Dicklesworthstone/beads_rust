# blocked_cache_table_missing

- **FM**: `fm-caches_indexes-blocked-cache-table-missing` (P0)
- **Subsystem**: caches_indexes
- **Detect**: `schema.tables` check goes to `error` with
  `Missing tables: blocked_issues_cache`. This is the structural twin of the
  FTS-missing false-negative bug (br-#152) — the detector must surface the
  finding, not spuriously OK.
- **Repair contract**: `--repair` reapplies the canonical schema via the
  JSONL→DB rebuild path (which itself calls `apply_schema` → `CREATE TABLE
  IF NOT EXISTS blocked_issues_cache` + the cache index). The fixer never
  DROPs.
- **Round-trip**: PARTIAL — DDL recreation routes via the rebuild path that
  predates full WP3/WP4 chokepoint coverage. `undo latest` may report
  `restored: 0` for the structural recreation; the fixture asserts the DB is
  still queryable post-undo.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
