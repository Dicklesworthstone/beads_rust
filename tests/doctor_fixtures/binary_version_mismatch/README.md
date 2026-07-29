# binary_version_mismatch

- **FM**: `fm-external_artifacts-binary-version-mismatch` (P1)
- **Subsystem**: external_artifacts
- **Detect**: `binary_version` check goes to `warn` when an
  upward-reachable `Cargo.toml` declares `name = "beads_rust"` and a
  `version` strictly GREATER than the running binary's
  `CARGO_PKG_VERSION`. Surfaces both versions + the canonical
  `cargo install --path . --locked` rebuild command.
- **Repair contract**: SAFETY — detect-only. The doctor cannot
  replace its own running binary; that is `br upgrade`'s job.
  Operator decides when to rebuild.
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0
