# jsonl_world_writable

- **FM**: `fm-permissions-jsonl-world-writable` (P2) — security
  concern when `.beads/issues.jsonl` has the world-write bit
  (`0o002`) set. Any local user could inject malicious issues that
  `br sync --flush-only` reimports.
- **Subsystem**: permissions
- **Detect**: `permissions.jsonl_world_writable` check goes to
  `warn` when the JSONL file's mode (`stat % 0o777`) has the world-
  write bit set.
- **Repair contract**: SAFETY — `--repair` strips the world-write
  bit (`current & !0o002`) via the `mutate()` chokepoint
  (`Op::Chmod`). Every other mode bit is preserved (owner-write,
  group-write, exec, sticky). This is the FIRST production
  exercise of `Op::Chmod`. The chokepoint backs up the file bytes
  verbatim with the ORIGINAL mode preserved via
  `copy_source_permissions`, so `doctor undo` byte-restores both
  content and mode through the existing WriteFile-with-mode
  restore path.
- **Round-trip**: chmod `.beads/issues.jsonl` to `0o666` → detect
  warn → `--repair` strips world-write (`0o664`) → re-detect ok →
  `doctor undo` restores `0o666`.
- **Idempotence**: a second `--repair` finds no divergence (the
  bit is already cleared); zero actions.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (mode bits cleared)
    - undo: 0 (mode restored)
- **Platform**: Unix-only. The fixture skips on non-Unix targets.
