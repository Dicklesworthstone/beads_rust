# blocked_cache_table_missing

- **FM**: `fm-caches_indexes-blocked-cache-table-missing` (P0)
- **Subsystem**: caches_indexes
- **Detect**: `schema.tables` check goes to `error` with
  `Missing tables: blocked_issues_cache`. This is the structural twin of the
  FTS-missing false-negative bug (br-#152) — the detector must surface the
  finding, not spuriously OK.
- **Repair contract**: For an otherwise admitted current-version layout,
  `--repair` uses `fm-schemas-current-runtime-maintenance` to recreate the
  derived cache and its index in place, inside the dedicated maintenance
  transaction, and mark the cache stale. The invocation reports remaining
  findings and ends without JSONL reconstruction or additional fixers.
- **Round-trip scope**: The repair is recorded in the doctor's reversible
  session. This fixture checks only that the main database still exists
  after best-effort undo; it does not assert byte-for-byte family restoration
  or cache repopulation.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (maintenance complete; other findings may remain)
    - undo: best-effort; the fixture checks that the database remains present
