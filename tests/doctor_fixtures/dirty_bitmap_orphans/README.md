# dirty_bitmap_orphans

- **FM**: `fm-caches_indexes-dirty-bitmap-divergence` (P2) — orphan
  row in the `dirty_issues` table.
- **Subsystem**: caches_indexes
- **Detect**: `dirty_bitmap` check goes to `warn` when one or more
  rows in `dirty_issues` reference an `issue_id` that no longer
  exists in `issues`. The FK `ON DELETE CASCADE` normally prevents
  this; orphans appear when an issue is deleted with FK enforcement
  off, or via a partial-rebuild path that bypasses the pragma.
- **Repair contract**: SAFETY — `--repair` issues a surgical
  `DELETE FROM dirty_issues WHERE issue_id NOT IN (SELECT id FROM
  issues)` via the `mutate()` chokepoint (`Op::DbExec` with
  `affected_tables=["dirty_issues"]` and predicate-based snapshotting).
  This is a targeted alternative to the heavy
  `repair_database_from_jsonl` rebuild; only the broken rows touch.
- **Round-trip**: insert one orphan dirty_issues row → detect warn
  → `--repair` deletes the orphan → re-detect ok → `doctor undo`
  reinstates the orphan row (issue_id + marked_at).
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (orphan row pruned)
    - undo: 0 (orphan row restored byte-deterministically)
