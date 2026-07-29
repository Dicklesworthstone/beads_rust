# config_yaml_secret_mode

- **FM**: `fm-permissions-config-yaml-mode-leaks-secrets` (P1) —
  `.beads/config.yaml` is world-readable AND contains secret-shaped
  keywords (`token`, `secret`, `password`, `api_key`, `private_key`).
- **Subsystem**: permissions
- **Detect**: `permissions.config_yaml_secrets` check goes to `warn`
  when both conditions are met (mode has 0o004 set AND the file
  contents contain a secret-keyword substring).
- **Repair contract**: SAFETY — `--repair` strips world-read AND
  world-write bits (`current & !0o006`) via the `mutate()`
  chokepoint (`Op::Chmod`). Owner and group bits are preserved —
  operators using group-shared configs keep working. This is the
  2nd production exercise of `Op::Chmod` (after cycle 25's
  jsonl-world-writable). The chokepoint backs up the file bytes
  verbatim with the ORIGINAL mode preserved, so `doctor undo`
  byte-restores both content and mode through the WriteFile-with-
  mode restore path.
- **Round-trip**: write a config.yaml with a secret keyword and
  chmod it `0o666` → detect warn → `--repair` strips world bits
  (`0o660`) → re-detect ok → `doctor undo` restores `0o666`.
- **Idempotence**: a second `--repair` finds no divergence; zero
  actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (mode tightened)
    - undo: 0 (mode restored)
- **Platform**: Unix-only. The fixture skips on non-Unix targets.
