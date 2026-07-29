# routes_jsonl_corrupt

- **FM**: `fm-routes_external-routes-jsonl-corrupt` (P1)
- **Subsystem**: routes_external
- **Detect**: `routes_jsonl` check goes to `warn` reporting the malformed
  line numbers + reason (parse_error / missing `prefix` / missing `path` /
  empty value). New detector landed in pass-2 to unblock this subsystem's
  fixture suite.
- **Repair contract**: SAFETY — detect-only. Doctor never auto-rewrites
  `.beads/routes.jsonl` because the operator's intent for cross-project
  routing is unknowable from the outside (dropping a malformed line could
  lose a routing decision the operator hasn't yet recorded elsewhere).
  Operator handles the rewrite by hand after reviewing.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
