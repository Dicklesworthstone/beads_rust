# config_unknown_keys

- **FM**: `fm-configs-unknown-keys`
- **Subsystem**: configs
- **Detect**: `config.unknown_keys` goes to `warn` and lists every top-level
  key of `.beads/config.yaml` that is not in the config key registry
  (`br config schema`), here the typo `defualt_priority` for
  `default_priority`. The
  `config.yaml` parse check stays `ok` because the file is valid YAML.
- **Repair contract**: SAFETY — detect-only. The doctor never rewrites the
  operator's `config.yaml`; a typo can only be fixed by the person who knows
  what they meant.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
