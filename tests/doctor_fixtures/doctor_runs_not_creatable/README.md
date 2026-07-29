# doctor_runs_not_creatable

- **FM**: `fm-permissions-doctor-runs-not-creatable` (P2) — the
  `.doctor/` directory at the repo root is not writable by the
  current user. The doctor's `--repair` flow creates a fresh
  `.doctor/runs/<run-id>/` directory for every run; a read-only
  parent makes this fail deep in the chokepoint with a cryptic
  `EACCES`.
- **Subsystem**: permissions
- **Detect**: `doctor.runs_creatable` check goes to `warn` when
  `.doctor/` exists but the owner-write bit is unset.
- **Repair contract**: **DETECT-ONLY** — operators may have
  intentionally locked `.doctor/` (compliance-controlled, group-
  shared workflows); auto-chmoding would stomp that decision.
  Doctor's job is to flag the problem up-front so the operator
  resolves it before re-running `--repair`. This fixture's
  `post_repair` stage asserts the SACRED INVARIANT: doctor must
  NOT silently chmod the directory.
- **Round-trip**: chmod `.doctor/` to `0o555` → detect warn →
  `--repair` runs (other fixers may fire) → assert `.doctor/`
  mode is unchanged → `doctor undo` → assert state restored.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: variable (depends on other findings; this FM doesn't
      contribute to the exit code because no fixer runs for it)
    - undo: 0 (state preserved)
- **Platform**: Unix-only.
