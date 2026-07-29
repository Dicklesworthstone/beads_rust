# `.githooks/` — opt-in pre-commit guards for beads_rust

This directory holds repository-local git hooks. They are **opt-in**:
git only honors them after you point `core.hooksPath` at this directory.

## Enable (one-time per clone)

```bash
git config core.hooksPath .githooks
```

This is per-clone and lives in `.git/config` — it never propagates with
the repository, so other contributors decide for themselves whether to
turn the hooks on.

## Disable

```bash
git config --unset core.hooksPath
```

Or skip a single commit without disabling globally:

```bash
BR_DOCTOR_SKIP_PRECOMMIT=1 git commit -m "emergency fix"
```

## What's here

### `pre-commit`

Runs `br doctor --quick --json` against the repo's `.beads/` workspace
before each commit. The fast path runs only the cheap detectors
(target: <1s on a healthy workspace). If the workspace is unhealthy,
the commit is blocked with a one-line summary and the recommended
`br doctor --repair --dry-run` next step.

Fail-open semantics:
- No `.beads/` in the repo root → exit 0 (nothing to check).
- `br` not on `PATH` → exit 0 with a stderr note.
- `BR_DOCTOR_SKIP_PRECOMMIT=1` → exit 0 (emergency bypass).

The hook never invokes `--repair` — it only diagnoses. Repair is the
operator's explicit, audited action via `br doctor --repair`.

## Why opt-in?

Per AGENTS.md, `br` is non-invasive: it never installs git hooks on
its own. This directory is the canonical place to wire a pre-commit
guard once you've decided you want one.
