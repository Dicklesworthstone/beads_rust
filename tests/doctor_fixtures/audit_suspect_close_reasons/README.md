# audit_suspect_close_reasons

- **FM**: `fm-agent_coordination-suspect-close-reason` (P2)
- **Subsystem**: agent_coordination
- **Detect**: `audit.suspect_close_reasons` check goes to `warn` (recently
  landed in commits 854e0382 + e8a5d2c2 — the audit policy for forced cycle
  closes that don't carry a dated `audit-historical-cycle-close-YYYY-MM-DD`
  label).
- **Repair contract**: SAFETY — no auto-fix. The audit policy is intentional:
  operator must either resolve the underlying cycle and reopen the bead, OR
  apply the dated audit label to acknowledge the historical close. `br doctor
  --repair` must NEVER silently add the label (that would defeat the audit).
- **Round-trip**: N/A — detect-only.
- **Expected exit codes**:
    - detect: 1 (findings present)
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
