# startup_cache_poisoned

- **FM**: `fm-configs-startup-cache-poisoned` (P2)
- **Subsystem**: configs
- **Detect**: `startup_cache.health` check goes to `warn` when the
  current workspace's resolved `startup-<key>.json` cache file is
  unreadable or fails to parse as a `StartupCacheRecord`. Other
  `startup-*.json` files in the cache dir are ignored because they may
  belong to unrelated workspaces.
- **Repair contract**: SAFETY — `--repair` renames the poisoned file
  into `<run-dir>/quarantine/startup-cache/` via the `mutate()`
  chokepoint. Per AGENTS.md RULE 1 the fixer NEVER deletes; rename
  means `doctor undo <run-id>` byte-reverses the move. The fixer
  extends `session.ctx.capabilities.write_scopes` with the cache
  directory because it lives outside the default workspace.
- **Round-trip**: prime real startup cache → poison that current-key file
  → detect warn → `--repair` quarantines only that file → re-detect ok →
  `doctor undo latest` restores the poisoned bytes at the original path.
- **Idempotence**: second `--repair` finds no poisoned files; zero
  actions emitted.
- **Expected exit codes**:
    - detect: 1 (warn present)
    - repair: 0 (poisoned file quarantined)
    - undo: 0 (poisoned file restored)
