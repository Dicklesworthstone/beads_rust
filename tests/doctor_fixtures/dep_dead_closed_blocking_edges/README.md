# dep_dead_closed_blocking_edges

- **FM**: `fm-dependencies-dead-closed-blocking-edges` (P3) and
  `fm-dependencies-fully-unblocked-open-issues` (P3) — the issue #350
  dependency-graph JSONL audit. One planted shape fires both: an open
  issue whose only `blocks` dependency targets a closed blocker has a
  dead edge, and — because every declared blocker is dead — it is also
  fully unblocked.
- **Subsystem**: dependencies
- **Detect**: `dep.dead_closed_blocking_edges` warns naming the open
  issue and its dead blocker; `dep.fully_unblocked_open` warns naming
  the open issue.
- **Repair contract**: DETECT-ONLY. Removing or updating a stale edge
  (`br dep remove`) is an operator decision; `--repair` must leave the
  planted graph untouched and both warnings truthfully present.
- **Plant**: pure public CLI — create two issues, `br dep add` the
  forward edge, `br close` the blocker, flush. No direct DB writes.
- **Expected exit codes**:
    - detect: 1 (warns present)
    - repair: non-zero tolerated (warnings persist by design)
    - undo: 0
