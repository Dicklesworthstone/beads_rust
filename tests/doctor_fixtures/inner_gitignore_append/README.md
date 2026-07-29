# inner_gitignore_append

- **FM**: `fm-configs-gitignore-leaking-beads` (P2, inner subset) —
  `.beads/.gitignore` is missing one or more of the canonical
  ephemeral patterns (`.write.lock`, `*.tmp`), causing transient
  workspace state to leak into git history.
- **Subsystem**: configs
- **Detect**: `gitignore.beads_inner_present` check goes to `warn`
  when the inner `.gitignore` is missing OR exists but doesn't list
  every canonical pattern.
- **Repair contract**: SAFETY — `--repair` appends the missing
  patterns via the `mutate()` chokepoint (`Op::AppendFile`).
  Symlinked `.beads/.gitignore` is REFUSED (operator intent may
  point at a vendored shared config). Existing operator-written
  lines are preserved verbatim; only the missing canonical lines
  are appended at end-of-file, with a separator newline inserted
  if the file's last byte is not `\n`.
- **Round-trip**: write a `.beads/.gitignore` with only one of two
  canonical patterns (`*.tmp` present, `.write.lock` missing) plus
  an operator-custom line → detect warn → `--repair` appends the
  missing canonical pattern → re-detect ok with operator lines
  preserved → `doctor undo` restores the incomplete state.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (canonical pattern appended)
    - undo: 0 (incomplete state restored byte-deterministically)
