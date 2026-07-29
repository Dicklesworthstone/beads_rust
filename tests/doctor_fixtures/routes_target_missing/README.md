# routes_target_missing

- **FM**: `fm-routes_external-route-target-missing` (P1)
- **Subsystem**: routes_external
- **Detect**: `routes.targets` check goes to `warn` enumerating each
  route whose `path` resolves to a directory that doesn't exist OR
  doesn't carry a recognizable `.beads`-style suffix. Each unresolved
  entry surfaces the prefix, declared `path`, the resolved absolute
  target, and the failure reason ("target is not a beads directory",
  "redirect loop", etc.).
- **Repair contract**: SAFETY — detect-only. The doctor never
  modifies `routes.jsonl` because the operator may have renamed the
  external project, moved the route to disk-encrypted storage, etc.
  Surface + advise; operator decides.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0

This fixture complements `routes_jsonl_corrupt` (which checks
shape) by exercising the per-route target-resolution path.
