# blocked_cache_stale

- **FM**: `fm-caches_indexes-blocked-cache-stale` (P0)
- **Subsystem**: caches_indexes
- **Detect**: `db.recoverable_anomalies` check goes to `warn`/`error` with the
  `blocked_issues_cache is marked stale and needs rebuild` finding (often
  paired with the content-mismatch finding because the ghost row makes the
  projection disagree with the live graph).
- **Repair contract**: `--repair` calls `SqliteStorage::rebuild_blocked_cache`,
  which now routes through the WP4 chokepoint (`Op::DbExec` /
  `Op::DbMigrate`). The stale marker is cleared and the ghost row is wiped
  in favour of the rebuilt projection.
- **Round-trip**: PARTIAL — the blocked-cache rebuild path is partially
  chokepointed; `undo latest` may report `restored: 0` for the inner SQL
  DELETE/INSERT pair. Test asserts the DB is still openable after undo.
- **Expected exit codes**:
    - detect: 1 (findings present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
