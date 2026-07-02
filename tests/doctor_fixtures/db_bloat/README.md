# db_bloat

- **FM**: `fm-caches_indexes-db-bloat-vs-jsonl`
- **Detector**: `db_bloat`
- **Severity**: warn
- **Repair contract**: `br doctor --repair` is detect-only by default. The
  fixture opts into `--unsafe-auto-fix --only fm-caches_indexes-db-bloat-vs-jsonl`
  so the harness exercises the explicit VACUUM path without weakening the
  doctor's normal safety posture.
- **Round-trip**: valid JSONL above the 1 MiB floor -> valid SQLite database
  with trailing reclaimable pages -> `db_bloat` warns -> unsafe repair VACUUMs
  the database and records `doctor.db_bloat_vacuum` -> undo restores the
  bloated database bytes.

