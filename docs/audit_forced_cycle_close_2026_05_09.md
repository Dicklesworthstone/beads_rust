# Audit: Forced-Cycle-Close Decision Matrix (2026-05-09)

**Operator:** audit-2026-05-09
**Bead:** beads_rust-30ci
**Trigger:** Audit synthesis subagent identified 5 closed beads with `close_reason` text matching "Forced close due to cycle" / similar pattern. The features are verifiably working in the current codebase, so the suspect part is the audit trail (text), not the code.

## Decision matrix

| # | Bead | P | Type | Title | Verifying evidence | Cycle status now | Decision | Rationale |
|--:|------|--:|------|-------|--------------------|------------------|---------:|-----------|
| 1 | `beads_rust-1ce` | P1 | epic | Phase 3: Relations & Search — Dependencies, Labels, Search | `br dep --help` / `br label --help` / `br comments --help` / `br search --help` all return well-formed help text. dep, label, comments, search commands all functional. Phase-3 epic is genuinely complete. | `br dep cycles` reports 0 cycles | **Option 3 — Accept as historical** | The close_reason actually summarizes the verified work (mentions "implemented and tested" — does not contain the bare "Forced close due to cycle" text). Lower priority for cleanup; tagged historical to surface the lineage. |
| 2 | `beads_rust-4n9` | P0 | feature | Unit Test Infrastructure | `tests/` directory has 130+ integration test files; `cargo test --release` runs (some failures fixed by sibling beads in this audit cycle but the test infra exists and runs) | `br dep cycles` reports 0 cycles | **Option 3 — Accept as historical** | Test infrastructure is verifiably present. Close-reason text "Implemented test infra. Forced close due to cycle." admits the cycle workaround but the work is real. |
| 3 | `beads_rust-aeb` | P1 | feature | list Command Implementation | `br list --json` returns valid JSON with `total` field; `tests/storage_ready.rs` exercises list-related code | `br dep cycles` reports 0 cycles | **Option 3 — Accept as historical** | List command is functional and tested. |
| 4 | `beads_rust-ap0` | P1 | feature | create Command Implementation | `br create --help` shows full option set including `--slug` (added 2026-05-07); `tests/e2e_basic_lifecycle.rs` exercises create flow | `br dep cycles` reports 0 cycles | **Option 3 — Accept as historical** | Create command is functional and tested. |
| 5 | `beads_rust-hvf` | P1 | feature | show Command Implementation | `br show --help` returns well-formed help; `tests/e2e_basic_lifecycle.rs` exercises show via CLI | `br dep cycles` reports 0 cycles | **Option 3 — Accept as historical** | Show command is functional and tested. |

## Action

Each bead is tagged with the `audit-historical-cycle-close-2026-05-09` label so:
- The future doctor rule `audit.suspect_close_reasons` (sibling bead `beads_rust-m3mi`) can use the label as its escape-hatch and not flag these.
- A future audit can find this triage decision via `br list --label audit-historical-cycle-close-2026-05-09`.

## Why not Option 1 (update close_reason in place)?

`br update` doesn't expose a `--close-reason` field; updating would require `br reopen` followed by `br close --reason <new>`, which would change `closed_at` to today's date and confuse historical trends. Option 3 (label-only) preserves history.

## Why not Option 2 (reopen + restructure)?

The cycle is no longer present (`br dep cycles` reports 0). Reopening would not help; the cycle that originally forced the close was resolved at some point. The architectural debt is paid.

## Why not Option 4 (reopen-as-incomplete)?

All 5 features are verifiably functional in the current codebase. Reopening would be incorrect.

## Cross-tool effects audit

After applying the historical labels:

```bash
br dep cycles --json | jq '.cycles | length'              # → 0 ✓
br epic status --json | jq '.[] | select(.id == "beads_rust-1ce") | .status'  # → "closed" ✓
bv --robot-triage 2>&1 | jq '.triage' >/dev/null && echo "bv OK"             # → bv OK ✓
br ready --json 2>&1 | jq 'length'                                            # → unaffected ✓
```

## Sibling bead reference

- `beads_rust-m3mi` (doctor `audit.suspect_close_reasons` check) — uses this label as the escape hatch to avoid flagging these 5 in future audits.
