# dependencies_orphans

- **FM**: `fm-caches_indexes-dependencies-orphans` (P2) — orphan-row
  in the `dependencies` table on either the `issue_id` side
  (FK-CASCADE bypassed) or the local `depends_on_id` side
  (referenced issue missing AND not external).
- **Subsystem**: caches_indexes
- **Detect**: `dependencies.orphans` check goes to `warn` when one
  or more rows in `dependencies` satisfy:
    - `issue_id` not in `issues`, OR
    - `depends_on_id` not in `issues` AND `depends_on_id` NOT LIKE
      `external:%` (external cross-repo refs are intentionally
      preserved).
- **Repair contract**: SAFETY — `--repair` issues a surgical
  `DELETE FROM dependencies WHERE <orphan predicate>` via the
  `mutate()` chokepoint (`Op::DbExec` with
  `affected_tables=["dependencies"]` and predicate-based
  snapshotting). External `depends_on_id` rows are NEVER deleted.
  Every affected row is snapshotted to `<run-dir>/backups/db/` so
  `doctor undo` byte-restores them.
- **Round-trip**: insert THREE orphan flavors plus an external-ref
  and a valid-link row → detect warn → `--repair` deletes the three
  orphans → re-detect ok with external + valid links preserved →
  `doctor undo` reinstates the three orphan rows.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (orphan rows pruned)
    - undo: 0 (orphan rows restored)
