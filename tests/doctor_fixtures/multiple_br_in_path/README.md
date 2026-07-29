# multiple_br_in_path

- **FM**: `fm-external_artifacts-multiple-br-in-path` (P1)
- **Subsystem**: external_artifacts
- **Detect**: `br_path_dupes` warns when more than one executable named `br`
  exists on `$PATH`, and the check details include the canonical FM id.
- **Repair contract**: detect-only. `br doctor --repair` must not rewrite or
  remove any discovered `br` binary. The operator must fix PATH ordering or
  stale installs manually.
- **Round-trip**: create two executable `br` stubs under fixture-local
  directories, prepend those directories only for doctor invocations, assert
  the detector fires, assert the stubs remain byte-identical after repair, and
  assert undo does not touch the stubs.

