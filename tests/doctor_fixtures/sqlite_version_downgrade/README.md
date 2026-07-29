# sqlite_version_downgrade

- **FM**: `fm-schemas-sqlite-version-downgrade`
- **Detector**: `refuse_gates.schema_version_downgrade`
- **Detect**: stamps `.beads/beads.db` with `PRAGMA user_version = 99`, above
  the binary's compiled schema version. `br doctor --repair --json` must refuse
  before creating a repair run or invoking any fixer.
- **Repair contract**: refuse-unsafe only. Doctor must not downgrade, rebuild,
  migrate, or otherwise mutate a database from a newer br schema.
- **Round-trip**: repair and undo are no-ops for the live database bytes; the
  header remains at `user_version == 99`.
- **Expected exit codes**:
    - detect: 0
    - repair: 4
    - undo: 0 or no-op failure ignored by the harness, with state unchanged

The source skeleton expected old `--fix` behavior plus an advisory artifact.
The live contract is narrower and safer: the refuse gate emits a structured
`refused_unsafe` envelope before any run-dir or repair action exists.
