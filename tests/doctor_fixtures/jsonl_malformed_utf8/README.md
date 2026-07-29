# jsonl_malformed_utf8

- **FM**: `fm-state_files-jsonl-malformed-utf8` (P1)
- **Subsystem**: state_files
- **Detect**: `jsonl.parse` check goes to `error` with
  `Failed to read JSONL: I/O error: stream did not contain valid UTF-8`.
- **Repair contract**: SAFETY — `--repair` MUST NOT silently rewrite the
  JSONL. The bytes might be operator data that requires hand review (or
  evidence of an external write-corruption that the operator needs to see
  before any rebuild). Doctor refuses; the planted invalid-UTF8 sequence
  must survive `--repair` byte-identically.
- **Round-trip**: N/A — no destructive auto-fix to undo.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0, 2, or non-zero (doctor may refuse explicitly); ANY exit
      that leaves the malformed bytes in place is acceptable.
    - undo: 0
