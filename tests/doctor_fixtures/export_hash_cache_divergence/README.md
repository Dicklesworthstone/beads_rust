# export_hash_cache_divergence

- **FM**: `fm-caches_indexes-export-hash-cache-divergence` (P1) —
  top-level hash variant
- **Subsystem**: caches_indexes
- **Detect**: `db.export_hash_cache` check goes to `warn` when
  `metadata.jsonl_content_hash` (the cached top-level fingerprint)
  differs from `compute_jsonl_hash(.beads/issues.jsonl)`. The JSONL on
  disk is authoritative; the cache is derived.
- **Repair contract**: SAFETY — `--repair` recomputes the JSONL hash
  and updates ONLY the `metadata.jsonl_content_hash` row via the
  `mutate()` chokepoint (`Op::DbExec` with `affected_tables=["metadata"]`).
  The JSONL itself is NEVER touched — the fixer would refuse if asked
  to mutate `.beads/issues.jsonl`. The pre-fix row is snapshotted as
  JSON under `<run-dir>/backups/db/metadata.jsonl` so `doctor undo`
  byte-restores it.
- **Round-trip**: corrupt the metadata row → detect warn → `--repair`
  rewrites the row to match the JSONL → re-detect ok → undo restores
  the corrupted value (JSONL hash unchanged throughout).
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (cache row updated)
    - undo: 0 (cache row reverted)
