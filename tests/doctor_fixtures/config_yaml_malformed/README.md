# config_yaml_malformed

- **FM**: `fm-configs-yaml-malformed` (P1)
- **Subsystem**: configs
- **Detect**: `config.yaml` check goes to `warn` reporting the
  `serde_yml::from_str` parse error message + a recommended_fix
  string telling the operator to open the file and fix it manually.
- **Repair contract**: SAFETY — detect-only. The doctor never
  rewrites the operator's `config.yaml` because there's no
  algorithmic way to know what they intended to write. The safe
  action is to surface the precise parse error.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
