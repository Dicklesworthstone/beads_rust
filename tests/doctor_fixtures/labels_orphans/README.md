# labels_orphans

- **FM**: `fm-caches_indexes-labels-orphans` (P2) — orphan-row in
  the `labels` table.
- **Subsystem**: caches_indexes
- **Detect**: `labels.orphans` check goes to `warn` when one or
  more rows in `labels` reference an `issue_id` that no longer
  exists in `issues`. Normally the FK `ON DELETE CASCADE` prevents
  this, but `PRAGMA foreign_keys = OFF` or a partial-rebuild path
  can leave orphans behind.
- **Repair contract**: SAFETY — `--repair` issues a surgical
  `DELETE FROM labels WHERE issue_id NOT IN (SELECT id FROM issues)`
  via the `mutate()` chokepoint (`Op::DbExec` with
  `affected_tables=["labels"]` and predicate-based snapshotting).
  Every affected row is snapshotted to `<run-dir>/backups/db/` so
  `doctor undo` byte-restores them via `restore_db_exec`.
- **Round-trip**: insert one orphan label row → detect warn →
  `--repair` deletes the orphan → re-detect ok → `doctor undo`
  reinstates the orphan row.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (orphan row pruned)
    - undo: 0 (orphan row restored)
