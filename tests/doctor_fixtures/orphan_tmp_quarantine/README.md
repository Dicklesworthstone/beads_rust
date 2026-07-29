# orphan_tmp_quarantine

- **FM**: `fm-state_files-orphan-tmp-files` (P2) — `.beads/*.tmp`
  or `.beads/*.tmp.<digits>` files older than 1 hour. Interrupted
  atomic-rename writers leave these behind; in-flight tmps live
  only milliseconds, so anything past the threshold is almost
  certainly orphaned.
- **Subsystem**: state_files
- **Detect**: `tmp_files_orphan` check goes to `warn` when one or
  more matching tmp files exist with `mtime` older than the
  threshold (`ORPHAN_TMP_AGE_THRESHOLD_SECS = 3600`).
- **Repair contract**: SAFETY — `--repair` renames each orphan tmp
  into `<run-dir>/quarantine/.beads/` via the `mutate()` chokepoint
  (`Op::Rename`). Per AGENTS.md RULE 1, no bytes are deleted; the
  files survive under the per-run quarantine. The fixer re-runs the
  age predicate at fix time as a TOCTOU defense — tmps that just
  landed (and are now legitimately in-flight) won't be moved even
  if they matched at detect time.
- **Round-trip**: plant `.beads/foo.tmp` with mtime 2h ago →
  detect warn → `--repair` quarantines the file → re-detect ok →
  `doctor undo` restores `.beads/foo.tmp` byte-deterministically.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (orphan quarantined)
    - undo: 0 (orphan restored byte-deterministically)
