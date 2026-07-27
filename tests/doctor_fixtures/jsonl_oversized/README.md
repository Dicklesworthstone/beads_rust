# jsonl_oversized

- **FM**: `fm-state_files-jsonl-oversized` (P3) — `.beads/issues.jsonl`
  exceeds the 100MB `jsonl_size` threshold. At that scale the sync
  engine's full-file read on every flush is slow and the in-memory
  parse pressures low-RAM hosts.
- **Subsystem**: state_files
- **Detect**: `jsonl_size` warns with `size_bytes` / `threshold_bytes`
  details. The padding is whitespace-only lines (skipped by every JSONL
  scanner), so `jsonl.parse` stays ok — the fixture isolates the size
  finding from content corruption.
- **Repair contract**: DETECT-ONLY. Compaction (closing stale issues,
  archiving comments, splitting the workspace) is operator-decided;
  `--repair` must never truncate or rewrite the file to silence the
  warning.
- **Cost note**: plants a ~105MB file in the fixture tempdir; written
  in one buffered pass and iterated as ~104 long lines, so the suite
  stays fast.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: non-zero tolerated (warning persists by design)
    - undo: 0
