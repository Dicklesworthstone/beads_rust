# schemas_missing_required_column

- **FM**: `fm-schemas-missing-required-column` (P1)
- **Subsystem**: schemas
- **Detect**: `schema.columns` reports the missing `comments.text`
  column as an error. The database still declares schema 19, but this readable
  layout is outside the reviewed upgrade and maintenance profiles.
- **Repair contract**: `--repair` exits 4 with `code: refused_unsafe`
  and gate `schema_current_layout_unsupported`. It must preserve the
  original database and JSONL, leave the missing column visible, and create
  no repair session or recovery artifacts. A supporting binary or separately
  reviewed migration is required.
- **Preservation**: after export, the fixture plants duplicate configuration
  and metadata rows with explicit rowids and attributed database-only event
  history. Every file under `.beads` must exactly match the captured byte
  baseline after detection, refused repair, optional replay, and best-effort
  undo. SQL assertions inspect a disposable database-family copy.
- **Undo**: refusal creates no repair run, so the harness's `undo latest`
  has nothing to undo and may exit nonzero. The unsupported source must remain
  byte-for-byte intact; no reconstruction is accepted.
- **Expected exit codes**: detect 1; repair 4; undo nonzero when no run exists.
