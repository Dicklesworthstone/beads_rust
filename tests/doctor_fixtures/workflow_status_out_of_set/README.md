# workflow_status_out_of_set

- **FM**: `fm-agent_coordination-workflow-status-out-of-set` (P3) —
  issue #311's read-side audit. Write-side enforcement rejects new
  out-of-set statuses, but rows written before the policy existed (or
  imported from external vocabularies) slip past it; this audit closes
  the gap.
- **Subsystem**: agent_coordination
- **Detect**: `policy.workflow_statuses` warns, naming the offender id
  and its out-of-set status (`limbo`), when `.beads/policy.yaml` has
  `workflow.strict: true` with a non-empty status set.
- **Repair contract**: DETECT-ONLY. Whether to migrate the offender
  into the set or widen the policy is an operator decision; `--repair`
  must not rewrite issue statuses.
- **Plant**: create two issues via CLI, write a strict
  `.beads/policy.yaml`, then set one issue's status to `limbo` via a
  direct DB UPDATE (mirrors an offender predating the policy).
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: non-zero tolerated (warning persists by design)
    - undo: 0
