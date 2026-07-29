# `br doctor` Real-World Fixture Suite (Phase 9)

This directory holds runnable fixtures that exercise `br doctor` against real
corrupt workspaces end-to-end:

```
corrupt.sh  →  br doctor --json  →  assert.sh (Stage A: detect)
            →  br doctor --repair --json  →  assert.sh (Stage B: post-repair)
            →  br doctor undo <first-repair-run-id> --json (best-effort round-trip)
```

Each fixture directory contains:

- `corrupt.sh <target_dir>` — deterministic recipe to plant the failure inside
  a fresh tempdir. Receives `TOOL_BIN` env (path to the `br` binary). Must
  leave the target in the planted-failure state. Captures a baseline snapshot
  under `<target_dir>/.fixture_baseline/` so the harness can verify what was
  planted survived doctor's read-only stages.
- `assert.sh <target_dir> <stage>` — invoked in two stages:
    - `assert.sh DIR detect` — runs `br doctor --json` and asserts the
      planted failure surfaces in the expected check name + status.
    - `assert.sh DIR post_repair` — invoked after `br doctor --repair`; asserts
      the failure is gone, quarantined, repaired, or truthfully still present
      for detect-only findings whose contract forbids an automatic fixer.
- `README.md` — one-paragraph description: what FM, what severity, expected
  detect status, expected exit codes.

## Round-trip caveat

`br doctor undo latest` only restores files the **chokepoint** (`mutate()`)
touched. Some current `--repair` paths predate WP3/WP4 chokepoint migration
(notably the JSONL→DB rebuild path) and route writes directly through `fsqlite`
or `std::fs`. For those fixtures, `undo latest` will report `restored: 0`
without failing — that is *not* a fixture failure, it is documented chokepoint
coverage. The `gitignore_leaking_beads` fixture *does* round-trip fully and
serves as the chokepoint regression test.

## Driver

`run_all.sh` is the bash driver. It iterates each fixture directory, sets up a
tempdir, runs the recipe, runs the assertions, runs `--repair` + assertions,
then undoes the first repair run and verifies it exits cleanly. Exits 0 if
every fixture passes; non-zero on first failure with a clear diagnostic.

Fixtures that need to exercise a public repair opt-in can write
`BR_DOCTOR_FIXTURE_REPAIR_ARGS=...` to `<target_dir>/.fixture_env` during
`corrupt.sh`. The runner appends those whitespace-separated arguments to both
repair invocations. This is for flags that are part of the doctor contract
itself, such as `--unsafe-auto-fix`; ordinary fixtures should keep the default
`br doctor --repair --json` path.

Per AGENTS.md: no `Command::new("git")` from runtime `br` code; the fixture
recipes themselves may call `git init` for setup (e.g. to materialise a real
`.git/info/exclude`).

## Idempotence replay gate (pass-3, opt-in)

`run_all.sh` supports an OPT-IN idempotence-replay gate between Stage 3
(`--repair`) and Stage 4 (`post_repair` assertions). When enabled, the harness
runs `--repair` a SECOND time on the already-repaired workspace and asserts
the new run-dir's `actions.jsonl` is empty — proving the fixer is idempotent
per the chokepoint contract:

```bash
REPLAY_IDEMPOTENCE=1 bash tests/doctor_fixtures/run_all.sh
```

The harness pins Stage 5 undo to the first repair run-id, so a no-op replay run
does not shadow the meaningful repair. The gate remains opt-in because some
fixtures are genuinely non-idempotent or intentionally detect-only.

**Per-fixture opt-out**: drop a `.skip_replay` marker file in the fixture
directory. The gitignore fixtures ship with one and document why.

**Suite-level opt-out**: `REPLAY_IDEMPOTENCE_SKIP="name1 name2"` to skip
specific fixtures regardless of the marker file.

The pass-3 design intent is to fold a per-fixture replay assertion INTO
each fixture's `assert.sh post_repair` stage in a future pass. Until then
the env-gated suite-level gate is the documented mechanism.

## Coverage manifest

[`COVERAGE.md`](COVERAGE.md) maps every declared `fm-` finding id to the
fixture(s) exercising it (empirical, beads_rust-yx0b). The drift gate
`tests/e2e_doctor_fixture_suite.rs::fixture_coverage_manifest_is_complete`
fails when a new finding id lacks a manifest row, a manifest row names a
missing fixture, or a fixture directory is never mentioned in the
manifest — update `COVERAGE.md` alongside new checks and fixtures.
