# br_history_growth

- **FM**: `fm-state_files-br-history-grows-unbounded` (P2)
- **Detector**: `br_history.size`
- **Subsystem**: state files
- **Shape**: `.beads/.br_history/` contains more than 100 recognized JSONL
  snapshot files.
- **Repair contract**: detect-only. History snapshots are operator recovery
  evidence; `br doctor --repair` must not prune or rewrite them.
- **Round-trip**: create 105 synthetic history snapshots -> detect warns ->
  `--repair` leaves the snapshots intact -> undo leaves the snapshots intact.

