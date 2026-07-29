# metadata_json_malformed

- **FM**: `fm-state_files-metadata-json-stale-or-malformed` (P1) —
  ParseError variant
- **Subsystem**: state_files
- **Detect**: A `metadata`/`metadata.json` check goes to `error` reporting
  `Failed to read metadata.json: JSON error`.
- **Repair contract**: SAFETY — `--repair` does NOT silently rewrite the
  malformed file. Operator review is required to prevent loss of any
  recoverable state encoded in the broken JSON. Either the planted bytes
  survive, or doctor writes a non-trivial replacement (never a silent
  truncation).
- **Round-trip**: N/A — no chokepointed auto-fix.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0, 2, or non-zero — any outcome that preserves the planted
      data is acceptable.
    - undo: 0
