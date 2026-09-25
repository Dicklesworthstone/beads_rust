# policy_unknown_keys

- **FM**: `fm-configs-policy-unknown-keys`
- **Subsystem**: configs
- **Detect**: `policy.unknown_keys` goes to `warn` and lists every key of
  `.beads/policy.yaml` that the typed policy schema does not recognise, with
  its dotted scope; here the typo `close_policy.require_close_reasn` for
  `require_close_reason` (GH #515). Ordinary commands also print the notice
  on stderr at default verbosity.
- **Repair contract**: SAFETY — detect-only. The doctor never rewrites the
  operator's `policy.yaml`; a typo can only be fixed by the person who knows
  what they meant.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
