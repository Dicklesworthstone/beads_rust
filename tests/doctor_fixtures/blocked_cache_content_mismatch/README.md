# blocked_cache_content_mismatch

- **FM**: `fm-caches_indexes-blocked-cache-stale` (P0) — content-mismatch variant
- **Subsystem**: caches_indexes
- **Detect**: `db.recoverable_anomalies` flags
  `blocked_issues_cache content differs from direct dependency graph` (and
  usually the matching `ready projection` finding) without the stale marker
  needing to be flipped. Exercises the projection-health probe.
- **Repair contract**: `--repair` forces a rebuild via
  `SqliteStorage::rebuild_blocked_cache(true)` because the report contains a
  projection-content-mismatch finding (`force_rebuild` path in
  `repair_recoverable_db_state`).
- **Round-trip**: PARTIAL — same chokepoint coverage gap as
  `blocked_cache_stale`.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2
    - undo: 0
