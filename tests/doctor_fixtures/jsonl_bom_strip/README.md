# jsonl_bom_strip

- **FM**: `fm-state_files-jsonl-utf8-bom-prefix` (P2) — a UTF-8 BOM
  (`0xEF 0xBB 0xBF`) at the start of `.beads/issues.jsonl` confuses
  some JSONL parsers (Go encoding/json strict-mode, jq with the
  wrong locale) and produces ambiguous diff output for the first
  record.
- **Subsystem**: state_files
- **Detect**: `jsonl_bom` check goes to `warn` when the first 3
  bytes of `.beads/issues.jsonl` match the UTF-8 BOM signature.
- **Repair contract**: SAFETY — `--repair` rewrites the file
  without the leading 3 bytes via the `mutate()` chokepoint
  (`Op::WriteFile`). The chokepoint captures the pre-rewrite bytes
  (BOM included) in a verbatim backup so `doctor undo` byte-
  restores the BOM-prefixed state.
- **Round-trip**: prepend BOM to a healthy JSONL → detect warn →
  `--repair` rewrites BOM-free → re-detect ok → `doctor undo`
  restores the BOM-prefixed bytes.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (BOM stripped)
    - undo: 0 (BOM restored byte-deterministically)
