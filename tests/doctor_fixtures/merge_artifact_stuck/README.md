# merge_artifact_stuck

- **FM**: `fm-state_files-merge-artifact-stuck` (P2)
- **Subsystem**: state_files
- **Detect**: `jsonl.merge_artifacts` check goes to `warn`
- **Repair contract**: SAFETY — `--repair` quarantines the stuck
  `.{base,left,right}.jsonl` artifacts (excluding the canonical
  `beads.base.jsonl` sync anchor) under
  `<run-dir>/quarantine/.beads/` via the `mutate()` chokepoint
  (Op::Rename). Per AGENTS.md RULE 1, the fixer NEVER deletes — it
  renames so `doctor undo <run-id>` byte-reverses the quarantine.
- **Round-trip**: corrupt → `--repair` → artifacts gone from .beads/ →
  `doctor undo latest` → artifacts restored.
- **Idempotence**: a second `--repair` finds no artifacts and is a no-op
  (zero actions.jsonl lines under REPLAY_IDEMPOTENCE=1).
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (artifacts quarantined)
    - undo: 0 (artifacts restored)
