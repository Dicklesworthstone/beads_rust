# inner_gitignore_append

- **FM**: `fm-configs-gitignore-leaking-beads` (P2, inner subset) —
  `.beads/.gitignore` is missing one or more of the canonical
  ephemeral patterns (`.write.lock`, `*.tmp`), causing transient
  workspace state to leak into git history.
- **Subsystem**: configs
- **Detect**: `gitignore.beads_inner_present` goes to `warn` when the inner
  `.gitignore` is missing or fails to cover required transient paths. Effective
  glob coverage such as `*.lock` satisfies `.write.lock`; later negation wins.
- **Repair contract**: SAFETY — `--repair` appends the missing
  patterns via the `mutate()` chokepoint (`Op::AppendFile`).
  Symlinked `.beads/.gitignore` is REFUSED (operator intent may
  point at a vendored shared config). Existing operator-written
  lines are preserved verbatim; only the missing canonical lines
  are appended at end-of-file, with a separator newline inserted
  if the file's last byte is not `\n`.
- **Round-trip**: write a `.beads/.gitignore` with `*.lock` (which effectively
  covers `.write.lock`) but no `*.tmp`, plus an operator-custom line → detect
  only the missing temp-file class → `--repair` appends `*.tmp` without a
  redundant literal `.write.lock` → re-detect ok with operator lines preserved
  → `doctor undo` restores the incomplete state.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (canonical pattern appended)
    - undo: 0 (incomplete state restored byte-deterministically)
