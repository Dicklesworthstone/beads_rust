# Examples (Agents)

This file shows small, copy/pasteable flows. For machine-readable examples, also see:

- `../../agent_baseline/examples/robot_mode_examples.jsonl`
- `agent_baseline/examples/`
- `scripts/agent_smoke_test.sh`

## List work (TOON -> JSON)

```bash
br ready --format toon --limit 10 | tru --decode --expand-paths safe | jq '.[0]'
```

## Update status (JSON)

```bash
br --json update br-abc123 --status in_progress | jq .
```

## Determinism smoke check

If the workspace is not changing, these should match:

```bash
br list --format json --limit 5 | jq -S . > a.json
br list --format json --limit 5 | jq -S . > b.json
diff -u a.json b.json
```
