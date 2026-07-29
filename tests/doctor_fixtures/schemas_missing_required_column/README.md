# schemas_missing_required_column

- **FM**: `fm-schemas-missing-required-column` (P1)
- **Subsystem**: schemas
- **Detect**: `schema.columns` check goes to `error` reporting the
  `comments.text` column missing. Confirms the detector watches the
  per-table column lists, not just the top-level required-tables set.
- **Repair contract**: `--repair` reapplies `apply_schema` via the JSONL→DB
  rebuild path, which restores the canonical comments schema (id, issue_id,
  author, text, created_at, …).
- **Round-trip**: PARTIAL — DDL recreation routes via the rebuild path;
  `undo latest` may report `restored: 0`. The fixture asserts the DB
  remains queryable post-undo.
- **Expected exit codes**:
    - detect: 1 (error present)
    - repair: 0 or 2 (rebuild_applied)
    - undo: 0
