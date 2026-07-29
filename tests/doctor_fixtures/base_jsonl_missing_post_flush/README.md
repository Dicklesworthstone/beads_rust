# base_jsonl_missing_post_flush

- **FM**: `fm-state_files-base-jsonl-missing-or-stale`
- **Detector**: `base_jsonl.missing_post_flush`
- **Severity**: warn
- **Repair contract**: detect-only. `br doctor --repair` must not invent a
  merge anchor when the only evidence is DB metadata saying a flush happened.
  Operators can regenerate the anchor through the normal sync path.
- **Round-trip**: fresh workspace with no anchor -> set
  `metadata.last_export_time` -> detector warns with
  `kind=missing_post_flush` -> repair is a no-op -> undo leaves the warning
  state intact.

