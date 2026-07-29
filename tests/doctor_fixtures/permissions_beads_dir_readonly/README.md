# permissions_beads_dir_readonly

- **FM**: `fm-permissions-beads-dir-readonly` (P0)
- **Subsystem**: permissions
- **Detect**: `permissions.beads_dir` check goes to `warn` enumerating
  every offending path + its current octal mode + the canonical
  `chmod u+w <path>` fix string.
- **Repair contract**: SAFETY — detect-only. The doctor NEVER `chmod`s
  the operator's `.beads/` files. The safe action is to surface the
  exact mode + the canonical fix command; the operator decides.
  Per-AGENTS.md: doctor must not mutate user permissions silently
  (could mask intentional read-only setups like compliance-locked
  audit DBs).
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0

This is the only fixture that intentionally `chmod`s its own
`.beads/issues.jsonl` to 0444 to trigger the detector. assert.sh
restores the mode in post_repair so TempDir cleanup works.
