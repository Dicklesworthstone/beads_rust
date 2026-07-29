# base_jsonl_stale_regen

- **FM**: `fm-state_files-base-jsonl-missing-or-stale` (P2, STALE
  subset) — `.beads/beads.base.jsonl` exists as a regular file but
  its `mtime` is older than `.beads/issues.jsonl`. A stale merge
  anchor produces incorrect 3-way merges when `br sync --merge`
  reconciles a remote with the local workspace.
- **Subsystem**: state_files
- **Detect**: `base_jsonl` check goes to `warn` when both:
  - `fs::symlink_metadata` on the anchor returns a regular file
    (not a symlink), AND
  - the anchor's `mtime` < the live JSONL's `mtime`, AND
  - the live JSONL is non-empty.
  Details payload carries `kind: "stale"` and `live_jsonl` path.
- **Repair contract**: SAFETY — `--repair` reads the current
  `.beads/issues.jsonl` bytes and rewrites `.beads/beads.base.jsonl`
  with those bytes via `chokepoint::mutate(Op::WriteFile)`. This
  matches what `br sync --flush-only` would produce as a side
  effect of a clean export; the doctor surfaces it as a named
  repair so operators don't need to remember which sync command
  rewrites the merge anchor.
- **Op variant**: `Op::WriteFile` (fourth WriteFile fixture, after
  cycles 18/43 BOM-strip, 24/47 CRLF-to-LF, and 27/46 inner-
  gitignore append; this one exercises the "rewrite-from-another-
  file-on-disk" shape — bytes come from a sibling read, not from
  in-memory transform of the source).
- **Action label**: `applied_actions` includes
  `base_jsonl_stale_regenerated`; `messages` includes
  `"Regenerated stale merge anchor from current JSONL."`.
  `actions.jsonl` records one `write_file` op under
  `fixer_id = doctor.base_jsonl_regen`.

TOCTOU defense: the fixer re-reads `issues.jsonl` at fix time and
refuses to regenerate if `issues.jsonl` has become empty between
detect and fix (would silently truncate the anchor and lose any
forensic value).

Combined with cycle 54 (symlink subset), this completes the
Tier-B → Tier-A graduation for both detector-emitted subsets of
the FM. The post_undo assertion verifies the chokepoint's verbatim
backup of the pre-regeneration stale anchor allows byte-deterministic
restoration via `doctor undo`.
