# no_db_mode_marker

- **FM**: `fm-state_files-no-db-mode-db-checks-skipped` (P4,
  informational) — issue #329's `--no-db` (JSONL-only) mode marker.
- **Subsystem**: state_files
- **Detect**: nothing is corrupt. The contract under test is that a
  reduced JSONL-only run (`br doctor --no-db --json`) announces its
  reduced scope explicitly: the `db.no_db_mode` check is present with
  status `ok`, the FM id inline, and the skipped DB-backed checks
  enumerated — while a full run carries no such marker.
- **Repair contract**: healthy workspace; `--repair` has nothing to do
  and must leave the workspace intact.
- **Why a fixture for an ok-status check**: the FM id is part of the
  declared finding-id contract surface; this pins that the marker (and
  its `finding_id` injection) does not silently disappear.
- **Expected exit codes**:
    - detect: 0 (healthy; marker asserted via explicit --no-db run)
    - repair: 0
    - undo: 0
