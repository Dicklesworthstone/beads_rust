# duplicate_config_rows

- **FM**: `fm-caches_indexes-*` (recoverable-anomalies family — duplicate
  `config` key variant).
- **Subsystem**: caches_indexes
- **Detect**: `db.recoverable_anomalies` goes to `error` with
  `config contains duplicate rows for key 'fixture.dup_test' (2 rows)`.
- **Repair contract**: `--repair` rebuilds the DB from JSONL. After
  rebuild, config holds exactly the canonical key/value pairs imported
  from JSONL.
- **Round-trip**: PARTIAL — same JSONL-rebuild chokepoint gap as the
  other db.recoverable_anomalies fixtures.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
