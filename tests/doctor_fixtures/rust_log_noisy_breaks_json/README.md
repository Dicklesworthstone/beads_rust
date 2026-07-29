# rust_log_noisy_breaks_json

- **FM**: `fm-observability-rust-log-noisy-breaks-json` (P1)
- **Subsystem**: observability
- **Detect**: `rust_log` check goes to `warn` reporting the active level
  and the recommended fix (`export RUST_LOG=error`).
- **Repair contract**: SAFETY — detect-only. The doctor cannot mutate the
  parent shell's environment; the operator must export `RUST_LOG=error`
  themselves (or wire it into their shell init / CI workflow).
- **Round-trip**: N/A — no chokepointed mutation.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2 (warning persists; no destructive action)
    - undo: 0

This is the only fixture that intentionally pokes the process env
(via the harness's `RUST_LOG` override) to trigger the detector — the
rest of the suite explicitly normalizes `RUST_LOG=error` to keep
output quiet.
