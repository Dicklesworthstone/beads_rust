# duplicate_metadata_rows

- **FM**: `fm-caches_indexes-*` (recoverable-anomalies family)
- **Subsystem**: caches_indexes (db.recoverable_anomalies surfaces duplicate
  rows in `config`, `metadata`, and `sqlite_master` for cache structures).
- **Detect**: `db.recoverable_anomalies` goes to `error` with
  `metadata contains duplicate rows for key 'jsonl_content_hash'`.
- **Repair contract**: `--repair` rebuilds the DB from JSONL. The rebuild
  reapplies the canonical schema and re-writes metadata (one row per key),
  so duplicates are squashed.
- **Round-trip**: PARTIAL — JSONL→DB rebuild predates full chokepoint
  coverage; `undo latest` may report `restored: 0`.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
