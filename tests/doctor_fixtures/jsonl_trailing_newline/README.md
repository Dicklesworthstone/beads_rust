# jsonl_trailing_newline

- **FM**: `fm-state_files-jsonl-missing-trailing-newline` (P2) —
  the selected JSONL export is non-empty but its last byte is not
  `\n`. POSIX text files should end with a newline; missing one
  causes `grep`, `jq -s`, `sed`, `wc -l`, and most line-oriented
  tools to silently skip or miscount the last record.
- **Subsystem**: state_files
- **Detect**: `jsonl_eof_newline` check goes to `warn` when the
  selected JSONL is non-empty and its trailing byte is not `\n`.
- **Repair contract**: SAFETY — `--repair` appends a single `\n`
  byte via the `mutate()` chokepoint (`Op::AppendFile`). The
  chokepoint captures the pre-rewrite bytes in a verbatim backup so
  `doctor undo` byte-restores the missing-newline state.
- **Round-trip**: strip trailing `\n` from a healthy JSONL → detect
  warn → `--repair` appends `\n` → re-detect ok → `doctor undo`
  removes the appended newline.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (newline appended)
    - undo: 0 (newline removed byte-deterministically)
