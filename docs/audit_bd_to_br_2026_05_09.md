# Audit: bd→br Fixture Migration Sweep (2026-05-09)

**Operator:** audit-2026-05-09
**Bead:** beads_rust-6plg
**Trigger:** Sibling bead `beads_rust-uelt` discovered `tests/repro_cache_crash.rs` used `bd-a/b/c` test fixture IDs in a `br` codebase. This bead audits the entire project for stale `bd-` references that need migration.

## TL;DR

**Migration is essentially complete.** The bd→br rename is done in user-facing surfaces (AGENTS.md, README.md, the failing test). Remaining `bd-` references are concentrated in:

1. **Perf artifacts** (~72,000 hits, 6 files in `tests/artifacts/perf/`) — frozen baseline test data; must NOT be touched.
2. **Illustrative-example test fixtures in `src/` `#[cfg(test)]` blocks** — internal-test fixtures using `bd-`-prefix as a generic placeholder; these don't appear in user-facing surfaces.
3. **Doc comments in source code** — illustrative IDs like `bd-epic.1`, `bd-abc.2` in rustdoc explaining behavior.
4. **`docs/CLI_REFERENCE.md`** — uses `bd-abc123` in `br show bd-abc123` examples to illustrate that `br` accepts any prefix (cross-tool tolerance).
5. **`docs/porting/PLAN_TO_PORT_BEADS_WITH_SQLITE_AND_ISSUES_JSONL_TO_RUST.md`** — historical document about the Go-to-Rust port; intentionally preserves the original `bd <command>` references.
6. **`docs/SYNC_SAFETY.md` line 237** — single reference to `bd sync` in a historical incident citation.

## Methodology

```bash
rg --no-heading -n 'bd-[a-z0-9]' tests/ src/ docs/ AGENTS.md README.md
```

**Raw count:** 76,291 hits across the project.
**After filtering `tests/artifacts/perf/`:** 2,551 hits.

## Per-category classification

| Category | Hit count | Action |
|----------|----------:|--------|
| **Perf artifacts** (`tests/artifacts/perf/*.toon`, 6 files × ~12,000 hits each) | ~72,000 | KEEP — frozen baseline test data |
| **`src/` doc comments** (e.g., `/// status annotations like "bd-123:open"`) | ~600 | KEEP — illustrative comments showing prefix-tolerance |
| **`src/` `#[cfg(test)]` fixtures** (e.g., `"bd-ready-summary"`, `"bd-1"`) | ~1500 | KEEP — internal test fixtures; not user-visible |
| **`docs/porting/`** (port-plan documents) | ~120 | KEEP — historical Go→Rust port docs preserve bd-syntax intentionally |
| **`docs/CLI_REFERENCE.md`** | 12 (`br show bd-abc123` examples) | KEEP — illustrating cross-tool prefix tolerance |
| **`docs/SYNC_SAFETY.md` line 237** | 1 | KEEP — historical incident citation about `bd sync` |
| **AGENTS.md, README.md** | 1 (mention of `bd-to-br-migration` skill) | KEEP — meta-reference to the skill |
| **User-facing legacy `bd <command>` syntax** | 0 | N/A — all migrated |

## Conclusion

The migration is functionally complete. The `bd-` references that remain in the codebase fall into three legitimate categories:

1. **Cross-tool tolerance**: br accepts `bd-`-prefixed IDs as a deliberate compatibility feature; tests verify this and docs illustrate it.
2. **Frozen baselines**: perf-test artifacts are immutable historical snapshots.
3. **Historical documentation**: port-plan docs and incident citations intentionally preserve the original syntax.

`tests/repro_cache_crash.rs` was the one true migration target, fixed by sibling bead `beads_rust-uelt`.

## Per the bd-to-br-migration skill

The skill at `~/.claude/skills/bd-to-br-migration/` defines the migration as:
- Section headers: `bd (beads)` → `br (beads_rust)` ✓ (done)
- Commands: `bd <X>` → `br <X>` ✓ (done in user-facing surfaces)
- Sync command: `bd sync` → `br sync --flush-only` ✓ (no remaining `bd sync` in user-facing docs; the 1 citation in SYNC_SAFETY.md is historical context)
- Issue IDs: `bd-###` → `br-###` ✓ (in test fixtures; the remaining bd- usage is illustrative)
- Daemon references / auto-commit / hook installation / RPC mode: ✓ (none in src/ or user-facing docs)

## Recommendation: do NOT add a CI lint for `bd-` references

A lint that fails on any `bd-` reference would be too aggressive — it would flag all the legitimate categories above. If a future regression is concerning (someone adds a NEW `bd <command>` example in user-facing docs), the better lint is targeted: grep for `\\bbd (sync|create|list|update|close|show|dep|ready|stats)\\b` in `docs/`, `AGENTS.md`, and `README.md`. That's the `bd-to-br-migration` skill's actual rule.
