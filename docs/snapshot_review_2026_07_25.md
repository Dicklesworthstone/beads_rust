# Snapshot Review Log — 2026-07-25

**Operator:** RubyHorse (claude-code)
**Bead:** `beads_rust-kmev` (insta snapshot golden refresh: list-limit default, clap help layout, doctor checks, bv 0.18 envelope)
**Trigger:** `cargo test --test snapshots` reported 12 failing goldens during a full-suite repair pass.

## Environment note (read this before reproducing)

`.snap.new` files written during an **rch remote** run stay on the worker and never
sync back, so `cargo insta` acceptance cannot be driven from a remote run. The
goldens below were regenerated locally with the real cargo, bypassing the
fail-closed offload wrapper at `/home/ubuntu/.local/bin/cargo`:

```bash
CARGO_TARGET_DIR=/data/tmp/br_local_target INSTA_UPDATE=new \
  "$HOME/.cargo/bin/cargo" test --test snapshots --no-fail-fast
```

Note also that `INSTA_UPDATE=no` suppresses `.snap.new` entirely — use `new`.

## Per-delta review

| # | Test | Cause class | Accept? | Rationale |
|--:|------|------------|--------:|-----------|
| 1 | `json_output::snapshot_list_json` | A | ✓ | `"limit": 50` → `0`, from `72ad01de` (#349) making `list`/`ready` complete by default. `src/cli/commands/list.rs:777` asserts `DEFAULT_LIST_LIMIT == 0`. |
| 2 | `json_output::snapshot_list_empty_json` | A | ✓ | Same single-line delta. |
| 3 | `json_output::snapshot_list_with_filters_json` | A | ✓ | Same single-line delta. |
| 4 | `json_output::snapshot_list_priority_ordering_json` | A | ✓ | Same single-line delta. |
| 5 | `json_output::representative_json_golden_list_output` | A | ✓ | Same delta inside the raw single-line payload; `total` still 3. |
| 6 | `toon_output::toon_golden_list_output` | A | ✓ | Same delta as `limit: 50` → `limit: 0` in TOON. |
| 7 | `cli_output::snapshot_doctor_output` | B + D | ✓ | Three changes, each accounted for — see below. |
| 8 | `cli_output::snapshot_create_help` | A + B | ✓ | clap's long-form help layout (dependency modernization, `fb6ed30a`) plus the genuinely new `--description-file` flag. |
| 9 | `cli_output::snapshot_help_output_no_mcp` | C | ✓ | **New snapshot.** `gate` subcommand added by #388; `serve` absent because `mcp` is off. See the feature-split note below. |
| 10 | `robot_output::robot_golden_bv_next_output` | A | ✓ | bv 0.18 added `actionable`, `phase2_ready`, `status`, `usage_hints` to the `--robot-next` envelope. |
| 11 | `robot_output::robot_golden_bv_triage_output` | A | ✓ | bv 0.18 inserted the nine-algorithm `triage.status` block. |
| 12 | `robot_output::robot_golden_bv_plan_output` | B | ✓ | Only `data_hash` changed; now masked (below), so the accepted golden is stable. |

### Delta 7 in detail — `snapshot_doctor_output`

| Line | Before | After | Class | Verdict |
|---|---|---|---|---|
| `rust_log` | `WARN … RUST_LOG=beads_rust=debug would dump verbose tracing…` | `OK rust_log: RUST_LOG=error (quiet)` | D | Test-fixture change. The harness now sets `RUST_LOG=error` (`tests/common/cli.rs`), which is exactly what this doctor check asks for. The old golden had captured the harness tripping the product's own check. |
| `binary_version` | `Running br 0.2.15; …` | `Running br X.Y.Z; …` | E | Now masked — see normalization fixes. |
| `db.sidecars` | `OK db.sidecars: WAL sidecar exists without a matching SHM sidecar … (expected for frankensqlite)` | `OK db.sidecars` | B | fsqlite 0.1.18 retains `-shm` after a clean exit, so the WAL-without-SHM branch (`src/cli/commands/doctor.rs:1868`) no longer fires. Status is unchanged (`ok`); only the informational message is gone. Not a regression. |

## Normalization fixes made while reviewing

Two goldens were drifting for reasons unrelated to any product change. Both were
fixed rather than re-accepted, so they stop churning:

1. **`tests/snapshots/mod.rs` — `VERSION_NUM_RE`.** The mask only covered the
   `version 0.1.7` form, so `br doctor`'s `binary_version` check (`Running br 0.2.19`)
   carried the crate version verbatim into the golden and broke it on **every
   release**. The regex now covers both forms and preserves which one matched.
   Covered by `test_mask_bare_br_version` and `test_mask_prerelease_version`.

2. **`tests/snapshots/robot_output.rs` — `DATA_HASH_RE`.** `data_hash` is bv's
   content fingerprint of the data it read — the same volatility class as
   `generated_at`, elapsed times, scores, and the bv version, all of which were
   already masked. It was simply missed. Freezing it adds no shape coverage and
   guarantees churn on any fixture edit. Covered by
   `normalize_bv_robot_output_masks_data_hash`.

## Feature-split note (delta 9)

`br --help` lists `serve` only when the optional `mcp` feature is compiled in, so
the top-level command list is feature-set dependent. CI runs
`cargo test --all-features` (`.github/workflows/ci.yml:88`) while the default
developer build leaves `mcp` off — a single golden cannot satisfy both, and
whichever build was not captured fails.

`tests/snapshots/cli_output.rs` now snapshots each feature set under its own name,
mirroring how `self_update` was already handled in the same file:

- `snapshot_help_output` → `help_output.snap` (requires `mcp`; also asserts `serve` is present)
- `snapshot_help_output_no_mcp` → `help_output_no_mcp.snap` (asserts `serve` is absent)

## Verdict

12 accepted, 0 rejected, **no class-R regressions found**. Every delta traces to a
named commit, a dependency upgrade, a harness change, or a masking gap that was
fixed rather than baked in.

Result: `snapshots` went from 12 failed to **252 passed / 0 failed**.

## Verification commands

```bash
# default feature set
CARGO_TARGET_DIR=/data/tmp/br_local_target \
  "$HOME/.cargo/bin/cargo" test --test snapshots

# the feature set CI uses, which exercises help_output.snap
CARGO_TARGET_DIR=/data/tmp/br_local_target \
  "$HOME/.cargo/bin/cargo" test --all-features --test snapshots
```
