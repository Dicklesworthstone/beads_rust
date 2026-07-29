# jsonl_duplicate_ids

- **FM**: `fm-state_files-jsonl-duplicate-ids` (P2) —
  `.beads/issues.jsonl` contains two or more records with the
  same top-level `id`. Usually the result of an unresolved merge
  conflict that left both sides' records, or a partial JSONL
  rebuild that double-appended.
- **Subsystem**: state_files
- **Detect**: `jsonl.duplicate_ids` check goes to `warn` when any
  id appears more than once. The check streams the file line-by-
  line and extracts just the leading `"id":"..."` substring per
  record (no full serde_json roundtrip).
- **Repair contract**: **DETECT-ONLY** — auto-fix is too risky.
  Deciding which copy is canonical depends on operator intent
  (timestamp? content hash? dependency graph?). Operators resolve
  by hand-editing the JSONL or by rebuilding from the DB. This
  fixture's `post_repair` stage asserts the SACRED INVARIANT:
  doctor must NOT pick a winner.
- **Round-trip**: hand-craft a JSONL with two records sharing
  `bd-aaa` and one unique record → detect warn → `--repair` runs
  → assert JSONL byte-checksum unchanged → `doctor undo` → assert
  state preserved.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: variable (other fixers may fire; this FM doesn't
      contribute to the exit code because no fixer runs for it)
    - undo: 0 (state preserved)
