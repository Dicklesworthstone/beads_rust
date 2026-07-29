# metadata_json_drift

- **FM**: `fm-configs-metadata-json-stale` (P1)
- **Subsystem**: configs
- **Detect**: `metadata.json` check goes to `warn` enumerating each
  declared field (`database` / `jsonl_export`) whose value names a
  file that doesn't exist on disk under `.beads/`. Each drift entry
  carries the field name, declared value, and expected_path so the
  operator can reconcile by either renaming the on-disk file or
  editing metadata.json.
- **Repair contract**: SAFETY — detect-only. The doctor never
  rewrites `metadata.json` because there's no algorithmic way to
  know which direction the operator wanted (rename file vs. edit
  metadata). The safe action is to surface the precise drift.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
