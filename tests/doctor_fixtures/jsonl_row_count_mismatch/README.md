# jsonl_row_count_mismatch

- **FM**: `fm-state_files-jsonl-row-count-mismatch` (P1)
- **Subsystem**: state_files
- **Detect**: `counts.db_vs_jsonl` check goes to `warn` reporting different
  record counts between the SQLite store and `issues.jsonl`. Details
  include `db` and `jsonl` integer counts. Confirms the JsonlFresher
  authority signal can fire under count drift.
- **Repair contract**: `--repair` may rebuild the DB from JSONL when the
  freshness gates allow it; either outcome (rebuild applied, or doctor
  refuses for safety) is acceptable. The fixture asserts that the JSONL
  file is never truncated by `--repair`.
- **Round-trip**: PARTIAL — rebuild path predates full chokepoint coverage.
- **Expected exit codes**:
    - detect: 1 (warning present)
    - repair: 0, 2, or non-zero (rebuild applied / refused for safety)
    - undo: 0
