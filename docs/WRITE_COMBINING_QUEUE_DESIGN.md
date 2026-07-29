# Write-Combining Queue Design

Status: design artifact only. The current production fallback remains one `br`
process per `.write.lock` acquisition.

## Purpose

Bursty swarm workloads often issue many small mutations at once: claims,
comments, status changes, labels, and short issue creates. Today every mutating
command separately acquires `.beads/.write.lock`, opens storage, applies any
needed recovery or import, mutates SQLite, and runs the usual JSONL flush path.
That conservative path is simple and correct, but under high agent counts the
lock handoff and startup cost can dominate the actual write.

The proposed write-combining path lets one lock holder accept a bounded queue of
compatible mutations, apply them sequentially while holding `.write.lock`, and
return the same per-command result each caller would have seen from the direct
path. The direct path must remain available for every command and must be the
automatic fallback on incompatibility, timeout, malformed requests, or disabled
combining.

## Non-Goals

- No implicit git operations.
- No background daemon started by normal CLI commands.
- No reordering for throughput in the first implementation.
- No compatibility shim for old command semantics.
- No weaker audit, dirty tracking, JSONL flush, or error reporting semantics.

## Current Write Path

The CLI currently resolves startup context, detects whether the command mutates
state, and acquires `.beads/.write.lock` before any database-family open that can
mutate or recover storage. The lock holder may then open storage, run
auto-import, execute the command, record events, and run auto-flush according to
the normal settings. This makes the lock boundary wider than the final mutation,
but it also protects recovery, schema work, sidecar quarantine, dirty tracking,
and fsqlite behavior that is not guaranteed to be read-only during startup.

Any combiner must preserve this boundary: it is a write-path accelerator, not a
way to bypass startup safety.

## Mutation Classes

The first implementation should treat compatibility as an allowlist. Unknown
commands, commands with file-system side effects, and commands whose output
depends on live process-local state must use the existing direct path.

### Never Combine

These commands should always run one process per lock:

- `init`
- `sync --import-only`
- `sync --merge`
- `sync --flush-only`
- `sync --import-only --rebuild`
- `sync --from`, `sync --to`, or other explicit external path forms
- `doctor --repair`
- configuration writes
- history restore, prune, or snapshot mutation
- commands that read arbitrary stdin as their primary payload
- commands that intentionally validate or rewrite JSONL files
- commands that mutate files outside `.beads/`
- commands whose success requires a distinct process lifetime or terminal state

Reason: these commands can change storage topology, import or export boundaries,
repair policy, external files, or user-visible side effects in ways that are not
equivalent to a simple queued SQLite mutation.

### Candidate After Proof

These commands are candidates only after direct-output parity tests and
failure-injection tests exist:

- homogeneous `create` bursts with explicit title, description, type, priority,
  labels, actor, and no stdin
- `comments add` with explicit message and actor
- `update` forms that change status, assignee, priority, title, description, or
  labels for an explicit issue id
- `close` and `reopen` for an explicit issue id
- dependency add/remove operations where both issue ids are explicit

The initial prototype should pick one homogeneous family, preferably the
contention replay lab's `create` workload, and prove equivalence before mixing
families.

### Read-Only Commands

Read-only commands should not enqueue. They should keep using the read path and
benefit from the existing read-only fast-open work. A combiner is useful only for
commands that would otherwise acquire `.write.lock`.

## Semantics to Preserve

Every queued mutation must behave as if it ran as a normal command after all
earlier accepted queue entries and before all later accepted queue entries.

- Audit event order follows accepted queue order exactly.
- Each event preserves the caller's actor, command identity, and timestamps as
  defined by the direct path.
- Each caller receives its own output payload and exit code.
- Validation errors are returned to only the failing caller.
- Successful earlier mutations are not hidden by a later queued failure.
- Dirty tracking records every successful mutation.
- JSONL auto-flush behavior remains honest: if SQLite committed but JSONL flush
  failed, the caller must see the same committed-but-flush-failed surface the
  direct path would report.
- Content hashes, issue ids, and deduplication inputs remain byte-for-byte
  equivalent to the direct path.
- Combining is bounded by request count, byte size, and caller deadline.

## Proposed Architecture

Combining should be explicit and default-off until proven by benchmarks.

1. An operator, test harness, or future MCP integration starts an explicit
   combiner process for one project.
2. The combiner acquires `.beads/.write.lock`, opens storage once, performs any
   startup recovery/import work allowed for normal mutating commands, and begins
   accepting local requests.
3. Writers submit `MutationEnvelope` values over a local transport with:
   command family, serialized arguments, actor, output mode, idempotency key,
   deadline, schema version, and caller response channel.
4. The combiner validates each envelope against the compatibility allowlist.
5. Accepted envelopes are applied sequentially in queue order while the combiner
   holds `.write.lock`.
6. The combiner returns a `MutationResult` per envelope with the same stdout,
   stderr class, structured result, exit code, and flush status the direct path
   would expose.
7. Rejected, expired, or incompatible envelopes fall back to the direct CLI path.

The transport can be a Unix domain socket on Unix hosts and a named pipe or
stdio-backed harness transport on platforms where sockets are not available. The
transport is intentionally an implementation detail; the compatibility and
result contracts are the durable design surface.

## Batch Boundaries

The combiner may drain more than one request per lock hold, but the first version
should still apply each request as a separate logical command.

- Use one queue order for all accepted requests.
- Use a small maximum batch size before any adaptive policy.
- Use per-envelope validation before mutation.
- Use a savepoint or equivalent per-envelope transaction boundary when the
  storage layer can support it cleanly.
- If per-envelope rollback cannot be guaranteed for a command family, exclude
  that family until the transaction boundary is proven.

This keeps the optimization targeted at avoiding repeated startup and lock
handoff costs, not at inventing new multi-command semantics.

## Failure Model

| Failure | Required behavior |
| --- | --- |
| Envelope parse or validation failure | Reject before side effects; caller may fall back to direct path. |
| Queue full or deadline exceeded | Reject before side effects; caller may fall back to direct path. |
| Incompatible command | Reject before side effects; caller uses direct path. |
| Mid-batch command validation failure | Return that command's validation error; preserve earlier committed successes. |
| Mid-batch storage failure | Roll back the current envelope; stop or shrink the batch; do not invent partial success. |
| Auto-flush failure after committed mutations | Report committed DB state plus flush failure to affected callers. |
| Combiner process exits before accepting a request | Caller falls back to direct path. |
| Combiner process exits after accepting but before response | Caller probes idempotency key or falls back only after proving the mutation did not commit. |

The idempotency key is required before any crash-recovery story can be correct.
It lets a caller distinguish "not accepted", "accepted and committed", and
"accepted but failed" without replaying a mutation that might create duplicate
events.

## Flush Strategy

The conservative strategy is:

1. Apply accepted envelopes in order.
2. Track whether any successful envelope would have requested auto-flush.
3. Run one normal JSONL flush after the successful portion of the batch.
4. Attach that flush result to every successful envelope that requested
   auto-flush.
5. If flush fails, return a failure result that says the database committed and
   JSONL export failed.

This avoids repeated JSONL exports during a burst while preserving the direct
path's honesty about SQLite versus JSONL state. Commands that explicitly request
`--no-auto-flush` should not be reported as flushed merely because a neighboring
queued command triggered export.

## Proof Plan

Proof should land before enabling any non-test user path.

1. Add unit tests for the compatibility classifier.
2. Add golden tests that run direct sequential commands and combined commands
   against fresh fixtures, then compare final issue JSON, event order, command
   outputs, and JSONL bytes.
3. Add failure-injection tests for invalid middle command, duplicate
   idempotency key, storage error, flush error, combiner crash before accept,
   and combiner crash after accept.
4. Extend the contention replay harness with a combined profile and compare
   lock wait, wall time, output hashes, replay determinism, and final JSONL hash
   against the direct profile.
5. Run the manual 64-worker contention profile before claiming a swarm-scale
   win.

Useful existing entry points:

```bash
cargo test --test bench_contention_replay -- --list
cargo test --test bench_contention_replay contention_ci_profile_records_and_replays_trace -- --nocapture
BR_CONTENTION_64=1 cargo test --test bench_contention_replay manual_64_worker_contention_profile_records_replayable_trace -- --ignored --nocapture
```

## Rollout Plan

1. Land this design and keep runtime behavior unchanged.
2. Add a pure compatibility classifier and envelope/result data model behind
   tests.
3. Build a test-only in-process combiner for one homogeneous command family.
4. Prove direct versus combined parity with golden and failure-injection tests.
5. Add an explicit opt-in local combiner process.
6. Add contention replay benchmarks comparing direct and combined profiles.
7. Consider MCP integration only after the explicit combiner has artifact-backed
   wins and a clean fallback story.

Combining must remain disableable with a config value or environment variable
while it is experimental.

## Acceptance Status

| Criterion | Status |
| --- | --- |
| Identify mutations that can and cannot be combined | Covered by this document. |
| Preserve per-command output and error surfaces | Required by design; prototype still needed. |
| Prove lock-wait reduction under bursty writes | Benchmark plan defined; measurement still needed. |
| Cover mid-batch and auto-flush failures | Failure model and proof plan defined; tests still needed. |
