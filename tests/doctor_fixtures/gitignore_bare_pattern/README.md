# gitignore_bare_pattern

- **FM**: `fm-configs-gitignore-leaking-beads` (variant: bare pattern, no trailing slash)
- **Severity**: P0
- **Subsystem**: configs
- **Detect**: `gitignore.beads_inner` check goes to `warn`
- **Repair contract**: Removes bare `.beads` line, preserves others.
- **Round-trip**: YES — chokepointed.
- **Expected exit codes**:
    - detect: 1
    - repair: 0 or 2
    - undo: 0

Confirms the detector handles both `.beads/` and `.beads` (no trailing slash).
