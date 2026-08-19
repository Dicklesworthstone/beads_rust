# dep_dead_closed_blocking_edges

- **FM**: `fm-dependencies-dead-closed-blocking-edges` (P3) and
  `fm-dependencies-fully-unblocked-open-issues` (P3) — the issue #350
  dependency-graph JSONL audit, re-scoped by issue #432. One planted
  shape exercises both: an open issue whose only `blocks` dependency
  targets a closed blocker has a SATISFIED dead edge, and — because
  every declared blocker is dead — it is also fully unblocked.
- **Subsystem**: dependencies
- **Detect (post-#432 contract)**: a present-but-closed blocker is the
  benign steady state of completed work, so BOTH checks report status
  `ok` while still carrying their FM id and a populated `details`
  payload (the `db.sidecars` / `no_db_mode_marker` shape): the
  dead-edge check lists the blocker under
  `issues[].satisfied_blockers` with `dangling_count == 0` and no
  `br dep remove` remediation; the fully-unblocked check lists the
  open issue under `ready` with an empty `stale_blocked` array.
  `Warn` is reserved for the real defects — a DANGLING edge (blocker
  absent from the JSONL) and a `blocked`-status issue with no live
  blocker — which are covered by unit tests in
  `src/cli/commands/doctor.rs` (`test_dep_graph_jsonl_*`) because
  planting a truly absent record requires surgery that `--repair`'s
  DB↔JSONL reconciliation would undo.
- **Repair contract**: DETECT-ONLY. `--repair` must leave the planted
  graph untouched and both informational payloads truthfully present.
- **Plant**: pure public CLI — create two issues, `br dep add` the
  forward edge, `br close` the blocker, flush. No direct DB writes.
- **Expected exit codes**:
    - detect: 0 (satisfied dependencies are not findings — #432)
    - repair: 0 tolerated non-zero (unrelated env warns may persist)
    - undo: 0
