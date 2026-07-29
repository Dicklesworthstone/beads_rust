# jsonl_crlf_to_lf

- **FM**: `fm-state_files-jsonl-crlf-line-endings` (P2) —
  `.beads/issues.jsonl` contains CRLF (`\r\n`) line endings.
  Streaming JSONL parsers may include `\r` as part of the JSON
  text, and `git diff` shows phantom whitespace changes.
- **Subsystem**: state_files
- **Detect**: `jsonl_crlf` check goes to `warn` when the first
  64KB of `.beads/issues.jsonl` contains any `\r\n` sequence.
- **Repair contract**: SAFETY — `--repair` rewrites the file with
  every `\r\n` sequence collapsed to `\n` via the `mutate()`
  chokepoint (`Op::WriteFile`). Standalone `\r` bytes (not
  followed by `\n`) are PRESERVED — the detector only flagged the
  CRLF pattern. The chokepoint captures the pre-rewrite bytes in a
  verbatim backup so `doctor undo` byte-restores the CRLF state.
- **Round-trip**: rewrite `.beads/issues.jsonl` with `\r\n` line
  endings → detect warn → `--repair` collapses to LF → re-detect
  ok → `doctor undo` restores CRLF bytes.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (CRLF normalised)
    - undo: 0 (CRLF restored byte-deterministically)
