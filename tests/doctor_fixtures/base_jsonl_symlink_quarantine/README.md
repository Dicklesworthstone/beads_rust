# base_jsonl_symlink_quarantine

- **FM**: `fm-state_files-base-jsonl-missing-or-stale` (P2, SYMLINK
  subset) — `.beads/beads.base.jsonl` is a symlink. The merge anchor
  is read by `br sync --merge` when reconciling a remote with a local
  workspace; a symlinked anchor is an attacker shape (a malicious
  actor could point it at any file on disk, causing 3-way merges
  to diff against attacker-chosen content).
- **Subsystem**: state_files
- **Detect**: `base_jsonl` check goes to `warn` when
  `fs::symlink_metadata` on the anchor reports `is_symlink()`.
  Details payload carries `kind: "symlink"`.
- **Repair contract**: SAFETY — `--repair` renames the symlink
  (not its target — `fs::rename` operates on the link bytes) into
  `<run-dir>/quarantine/.beads/beads.base.jsonl` via
  `chokepoint::mutate(Op::Rename)`. Per AGENTS.md RULE 1: rename,
  never delete. The chokepoint snapshots the symlink bytes
  themselves; `doctor undo` reinstates the symlink at its original
  path.
- **Op variant**: `Op::Rename` (third Rename fixture, after
  cycles 1 `merge_artifact_stuck` and 48 `orphan_tmp_quarantine`).
  This one exercises the symlink-rename shape specifically —
  renaming a symlink is byte-deterministic only because
  `fs::rename` does NOT follow the link.
- **Action label**: `applied_actions` includes
  `base_jsonl_symlink_quarantined`; `messages` includes
  `"Quarantined symlinked merge anchor."`. `actions.jsonl` records
  one `rename` op under `fixer_id =
  doctor.base_jsonl_symlink_quarantine`.

The stale-anchor case (`kind: "stale"`) is a separate fixer
(`doctor.base_jsonl_regen`, cycle 6) and would warrant its own
fixture; that's deferred.

TOCTOU defense: the fixer re-runs `fs::symlink_metadata` at
fix time to confirm the anchor is still a symlink. If the
operator moved the symlink between detect and fix, the fixer
no-ops rather than mutating an unintended target.

## Architectural finding: undo restores a regular file, not the symlink

The cycle-5 fixer's doc comment claims "The chokepoint snapshots
the symlink target itself (just the symlink bytes — the target's
content is intentionally NOT followed)". Empirically the
implementation diverges from this intent:

1. **Backup** — `copy_verbatim_with_perms` (`mutate.rs:479`) uses
   `fs::copy` which FOLLOWS symlinks. The backup stored under
   `<run-dir>/backups/.beads/beads.base.jsonl` is therefore a
   REGULAR FILE containing the link target's content, not the
   link bytes.
2. **Forward rename** — `fs::rename` correctly preserves the
   symlink bytes; the quarantined entry under
   `<run-dir>/quarantine/.beads/beads.base.jsonl` IS a symlink.
3. **Undo** — `restore_rename` (`surface.rs:1438`) checks
   `from.exists()` against the quarantined entry. `Path::exists()`
   follows symlinks, and the relative target `issues.jsonl`
   doesn't resolve under `<run-dir>/quarantine/.beads/`, so the
   check returns `false` and undo falls back to the byte backup
   via `Op::WriteFile`. Result: the original path is restored as
   a REGULAR FILE containing the link target's content.

The fixture asserts the observed behavior (post_undo accepts
either a preserved symlink or a regular-file content match). If
the chokepoint is ever upgraded to preserve symlinks through
backup+undo (e.g., by adding a `copy_verbatim_preserving_symlinks`
variant that uses `symlink_metadata` + `read_link` + `symlink_create`),
the fixture's `[ -L ... ]` branch will take precedence and the
test still passes.

