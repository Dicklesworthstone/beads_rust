# dirty_flag_divergence

Promotes `fm-state_files-dirty-flag-divergence`: a workspace whose `dirty_issues`
table says one issue still needs export even though the JSONL has already been
flushed. Current `br doctor` surfaces this through the `sync.metadata` check.
The current repair surface has no surgical fixer for this FM, so the fixture
asserts truthful detect-only behavior: `--repair --only
fm-state_files-dirty-flag-divergence` must leave the dirty row intact and report
`verified=false` instead of claiming success while the warning remains.
