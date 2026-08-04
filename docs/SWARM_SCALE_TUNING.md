# Swarm-Scale Tuning

This guide is for running `br` on high-RAM, high-core agent hosts, especially
machines with 256GB+ RAM and 64+ CPU cores. The operating model is conservative:
keep the issue tracker local, keep writes explicit, use Agent Mail for edit
reservations, and collect evidence before enabling any new adaptive path.

Validation note: commands marked "validated" were smoke-checked on
2026-05-03. Commands marked "manual" are intentionally host-specific, expensive,
or require an MCP client.

## Baseline Mode

Use this profile for ordinary agent panes and CI-style automation.

Validated:

```bash
RUST_LOG=error br ready --json --limit 1
RUST_LOG=error br sync --status --json
RUST_LOG=error br schema all --format json
```

Default recommendations:

- Prefer `--json` or `--robot` for all machine parsing.
- Set `RUST_LOG=error` unless debugging `br` internals.
- Keep mutation commands explicit. `br sync` does not run Git, and `br` does
  not install hooks or daemons. Git visibility is an explicit
  `br vcs-status` request.
- Use `br sync --status --json` as a cheap pre-commit state check.
- Use `br sync --flush-only` at the end of a mutation session before staging
  `.beads/issues.jsonl`.

Manual, optional shell profile:

```bash
export RUST_LOG=error
export BR_AGENT_NAME="${AGENT_NAME:-agent}"
```

## High-Core Build Hygiene

On a 64-core swarm host, the easiest way to waste the machine is letting every
agent compile into the same target directory. Give each pane or agent its own
target directory.

Manual, choose a local fast disk with enough free space:

```bash
export CARGO_TARGET_DIR="/data/tmp/br-target-${AGENT_NAME:-agent}"
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Manual, fleet-specific `rch` profile:

```bash
RCH_TARGET="/data/tmp/br-rch-target-${AGENT_NAME:-agent}"
rch exec -- env CARGO_TARGET_DIR="$RCH_TARGET" cargo check --all-targets
```

Do not delete target directories from an agent pane unless the operator has
explicitly approved the exact cleanup command. Prefer creating a fresh target
directory when disk pressure is not urgent.

## Lock Timeouts

`br` serializes local mutations through `.beads/.write.lock`. On large swarms,
short timeouts are useful for probes and long timeouts are useful for committed
write work.

Validated flag discovery:

```bash
br --help | rg -- '--lock-timeout|--db|--no-db'
```

Recommended profiles:

| Profile | Use | Setting |
| --- | --- | --- |
| Probe | read-heavy diagnostics that should fail fast if they accidentally write | `--lock-timeout 1000` |
| Normal agent | ordinary claim/comment/close operations | default timeout |
| Bulk maintenance | planned sync/import/export or migration work | `--lock-timeout 60000` |

Manual, replace `<id>` with the bead being claimed:

```bash
RUST_LOG=error br update <id> --claim --lock-timeout 60000 --json
```

Validated inspection commands:

```bash
RUST_LOG=error br sync --status --json
git status --short
```

Read-heavy commands use a current-schema read-only fast-open path when it can
avoid startup recovery and writer-lock work. If you need to compare behavior
against the conservative locked path, or temporarily disable the optimization
while debugging, set:

```bash
BR_DISABLE_READ_ONLY_FAST_OPEN=1 RUST_LOG=error br list --json --limit 1
```

Only truthy values (`1`, `true`, `yes`, `on`) disable the fast path. Unset or
false-like values keep the default fast-open behavior.

## Agent Mail And Beads

Use `br` for task state and Agent Mail for collision avoidance.

Manual workflow sequence:

```text
1. br ready --json
2. Agent Mail file_reservation_paths(..., reason="<bead-id>")
3. br update <bead-id> --claim --json
4. Work in the reserved files
5. br close <bead-id> --reason "<summary>" --json
6. br sync --flush-only
7. Agent Mail release_file_reservations(...)
```

Reservation scope should be narrow. Prefer exact files over `src/**`; reserve
the test file you create and the implementation file you edit. For generated
performance artifacts, reserve the exact run directory pattern before writing.

Manual degraded-coordination fallback, only when Agent Mail is unavailable:

```bash
br update <id> --status in_progress --assignee "$AGENT_NAME" --json
br comments add <id> --author "$AGENT_NAME" \
  --message "degraded-coordination: Agent Mail unavailable; files: src/foo.rs, tests/foo.rs" \
  --json
```

This is an advisory claim, not a lock. Keep the file set small and re-check
`git status --short` before editing.

## Coordination Console Runbook

Use this runbook when a swarm pane reports no ready work, but the backlog still
has open or blocked follow-up work. The goal is to distinguish a truly dry queue
from work hidden behind stale `in_progress` claims.

Manual command shape:

```bash
RUST_LOG=error br ready --json
bv --robot-next
RUST_LOG=error br list --status in_progress --json
RUST_LOG=error br coordination status --json
```

Interpretation:

- Empty `br ready --json` plus empty `in_progress` output means the ready queue
  is actually dry; move to `bv --robot-alerts` or backlog planning.
- Empty ready output plus `in_progress` claims means inspect claim age,
  assignee, latest comments, and Agent Mail evidence before reclaiming.
- `classification: "no_mail_snapshot"` means gather Agent Mail evidence; it is
  not abandonment proof.
- `required_human_confirmation: true` means ask the owner or operator before
  touching the bead.

Manual snapshot correlation, using files exported by the coordination layer
outside `br`:

```bash
RUST_LOG=error br coordination status \
  --reservations reservations.jsonl \
  --agents agents.jsonl \
  --json
```

Safe reclaim remains a two-step manual sequence. Review advisory output first,
then copy the suggested audit comment and claim command only when policy allows
it:

```bash
RUST_LOG=error br coordination status --reservations reservations.jsonl --agents agents.jsonl --json \
  | jq '.claims[] | select(.reclaim_allowed_by_policy == true) | {id: .issue.id, suggested_commands}'
```

`br coordination status` is read-only. It does not call Agent Mail, run git,
create reservations, release reservations, or auto-reclaim work.

## MCP Serve Topology

`br serve` is optional and requires the `mcp` feature. It runs over stdio, not a
TCP port, and it uses the same SQLite/JSONL workspace and lock model as the CLI.

Validated feature discovery:

```bash
cargo metadata --format-version=1 --no-deps | jq '.packages[0].features | keys'
```

Manual build and client launch:

```bash
MCP_TARGET="/data/tmp/br-mcp-target-${AGENT_NAME:-mcp}"
cargo build --release --features mcp --target-dir "$MCP_TARGET"
RUST_LOG=error "$MCP_TARGET/release/br" serve --actor "${AGENT_NAME:-mcp}"
```

Topology guidance:

- One MCP server process per active workspace is usually enough.
- Use MCP for agents that benefit from discoverable tools/resources/prompts.
- Use direct CLI calls for simple scripts and batch shell pipelines.
- Agent Mail remains the reservation layer. MCP is an API surface, not a lock
  manager.
- `beads://coordination/status` mirrors `br coordination status --json` for
  MCP-native agents, but it has no live Agent Mail access; use CLI snapshots for
  reservation correlation.

## Evidence Workflow

Any performance claim should carry three things:

- Golden behavior evidence: output shape, hash, or schema comparison.
- Resource evidence: timing, RSS, syscall, or lock-wait data.
- Reproduction metadata: command, seed, target binary, and artifact path.

Validated smoke commands:

```bash
cargo test --test bench_contention_replay -- --list
cargo test --test bench_synthetic_scale -- --list
```

Current reusable evidence entry points. Test discovery was validated with the
`-- --list` commands above; run the listed commands when you need the matching
artifact.

| Artifact | Command | Status |
| --- | --- | --- |
| Performance evidence ledger smoke | `cargo test --test bench_cold_warm_start perf_evidence_smoke_bundle_records_list_json_command -- --nocapture` | Manual smoke artifact run |
| CI contention replay | `cargo test --test bench_contention_replay contention_ci_profile_records_and_replays_trace -- --nocapture` | Manual smoke artifact run |
| Manual 64-worker contention profile | `BR_CONTENTION_64=1 cargo test --test bench_contention_replay manual_64_worker_contention_profile_records_replayable_trace -- --ignored --nocapture` | Manual 64-core host run |
| Bounded synthetic RSS profile | `BR_E2E_STRESS=1 BR_SYNTHETIC_EVIDENCE_ISSUES=1024 cargo test --test bench_synthetic_scale stress_synthetic_evidence_profile -- --ignored --nocapture` | Manual stress artifact run |
| Million-issue synthetic corpus generator | `BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1 cargo test --test bench_synthetic_scale stress_synthetic_million -- --ignored --nocapture` | Manual high-RAM corpus run |

The manual 64-worker and synthetic profiles are intentionally expensive. Run
them on a host sized for the workload, with an explicit artifact directory when
you need to preserve a proof bundle.

Manual bounded synthetic run:

```bash
ARTIFACT_DIR="tests/artifacts/perf/beads-perf-$(date -u +%Y%m%dT%H%M%SZ)-swarm"
BR_E2E_STRESS=1 \
BR_SYNTHETIC_EVIDENCE_ISSUES=1024 \
BR_SYNTHETIC_EVIDENCE_DIR="$ARTIFACT_DIR" \
cargo test --test bench_synthetic_scale stress_synthetic_evidence_profile -- --ignored --nocapture
```

## High-RAM Policy

Do not spend RAM by default just because the host has it. Large resident caches
should be opt-in and evidence-gated.

Use this rollout ladder for snapshot, cache, graph-projection, and adaptive
controller features:

1. Direct path only. Record baseline latency, RSS, and output hashes.
2. Shadow mode. Compute the candidate result but serve the direct result.
3. Advisory mode. Emit candidate decision evidence while direct fallback remains
   available.
4. Opt-in serve mode. Enable only for the workspace or MCP process that needs
   it.
5. Default-on consideration. Only after a perf bundle proves a win and parity
   tests cover stale, corrupted, and routed workspaces.

Rollback rule: every adaptive path must have a direct serial fallback. If a
snapshot/cache/controller reports stale state, missing evidence, parity drift, or
memory-budget pressure, disable it and use the direct SQLite/JSONL path.

## Failure Recovery

For swarm failures, preserve evidence first.

Validated diagnostic commands:

```bash
RUST_LOG=error br sync --status --json
RUST_LOG=error br doctor --json
git status --short
```

Then classify:

| Symptom | First response |
| --- | --- |
| Write lock timeout | Inspect active panes and Agent Mail reservations; do not kill processes blindly. |
| JSONL newer than DB | Use `br sync --status --json` and decide between import/merge paths explicitly. |
| DB newer than JSONL | Run `br sync --flush-only` before staging `.beads/issues.jsonl`. |
| Snapshot/cache mismatch | Disable the adaptive path and rerun the direct command. |
| Controller chose a surprising path | Keep its evidence output, switch to conservative mode, then file a bead with the artifact path. |

No recovery step should delete `.beads/` artifacts as a first move. History,
temp files, and recovery directories are diagnostic evidence until the operator
explicitly decides otherwise.

## Closeout Checklist

Validated closeout command shape:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
br sync --status --json
```

Also run:

- Focused tests for the changed command or module.
- UBS on the exact changed files.
- A perf/evidence command when the change makes a performance claim.
- Agent Mail completion plus file reservation release.

Do not mark a performance bead complete from green tests alone. The evidence
must cover the command shape, output shape, resource metric, and rollback story
claimed by the bead.
