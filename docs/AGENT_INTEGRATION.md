# AI Agent Integration Guide

This guide covers how AI coding agents can effectively use `br` (beads_rust) for issue tracking and workflow management.

---

## Table of Contents

- [Overview](#overview)
- [Quick Start for Agents](#quick-start-for-agents)
- [JSON Mode](#json-mode)
- [Workflow Patterns](#workflow-patterns)
- [Parsing JSON Output](#parsing-json-output)
- [Error Handling](#error-handling)
- [Robot Mode Flags](#robot-mode-flags)
- [Agent-Specific Configuration](#agent-specific-configuration)
- [Best Practices](#best-practices)

---

## Overview

`br` is designed with AI coding agents in mind:

- **JSON output** for all commands (`--json` flag)
- **Machine-readable errors** with structured error codes
- **Non-interactive** - no prompts, no TUI in normal operation
- **Deterministic** - same input produces same output
- **Fast** - millisecond response times for most operations

### Key Principles

1. **Always use `--json`** for programmatic access
2. **Check exit codes** for success/failure
3. **Parse structured errors** for recovery hints
4. **Use `br ready`** to find actionable work
5. **Sync at session end** with `br sync --flush-only`

---

## Quick Start for Agents

```bash
# Initialize (if needed)
br init --prefix myproj

# Find work
br ready --json --limit 5

# Claim and work
br update bd-123 --claim --json
# ... do the work ...
br close bd-123 --reason "Implemented feature X" --json

# Create discovered work
br create "Found bug during implementation" -t bug -p 1 --deps discovered-from:bd-123 --json

# Session end
br sync --flush-only
```

---

## JSON Mode

### Enabling JSON Output

```bash
# Flag on any command
br list --json
br show bd-123 --json
br create "Title" --json

# Equivalent (when the command supports --format)
br list --format json
br ready --format json

# Robot mode alias (same as --json)
br ready --robot
br close bd-123 --robot
```

### TOON Output (Token-Efficient)

Many read-style commands support TOON output via `--format toon`:

```bash
br ready --format toon --limit 10
br show bd-123 --format toon
```

Decode TOON to JSON when you need to pipe into JSON tools:

```bash
br ready --format toon --limit 10 | tru --decode | jq '.[0]'
```

### Environment Defaults

If you omit `--format` / `--json`, br can default the output format via env vars:

- `BR_OUTPUT_FORMAT` (highest precedence)
- `TOON_DEFAULT_FORMAT` (fallback)

Example:

```bash
export TOON_DEFAULT_FORMAT=toon
br list --limit 5          # defaults to TOON
br list --json --limit 5   # JSON always wins
```

### JSON Output Characteristics

- **Always valid JSON** - parseable even on errors
- **Arrays for lists** - `br list`, `br ready`, `br search`
- **Objects for single items** - `br show`, `br create`
- **Structured errors** - error object with code and hints

### Example Output

```bash
$ br ready --json --limit 2
```
```json
[
  {
    "id": "bd-abc123",
    "title": "Implement user auth",
    "status": "open",
    "priority": 1,
    "issue_type": "feature",
    "assignee": "",
    "dependency_count": 0,
    "dependent_count": 2
  },
  {
    "id": "bd-def456",
    "title": "Fix login bug",
    "status": "open",
    "priority": 0,
    "issue_type": "bug",
    "assignee": "alice",
    "dependency_count": 1,
    "dependent_count": 0
  }
]
```

---

## Workflow Patterns

### Standard Agent Workflow

```
┌─────────────────────────────────────────────────────────────┐
│  1. DISCOVER                                                │
│     br ready --json                                         │
│     → Find unblocked, undeferred issues                     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  2. CLAIM                                                   │
│     br update <id> --claim --json                           │
│     → Sets assignee + status=in_progress atomically         │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  3. WORK                                                    │
│     Implement the task...                                   │
│     → If you find new work:                                 │
│       br create "New issue" --deps discovered-from:<id>     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  4. COMPLETE                                                │
│     br close <id> --reason "Done" --json                    │
│     → Optionally: --suggest-next for chained work           │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│  5. SYNC (at session end)                                   │
│     br sync --flush-only                                    │
│     → Export to JSONL for git collaboration                 │
└─────────────────────────────────────────────────────────────┘
```

### Claiming Work

```bash
# Atomic claim (recommended)
br update bd-123 --claim --json

# Manual claim (equivalent)
br update bd-123 --status in_progress --assignee "$BD_ACTOR" --json
```

### Creating Related Issues

```bash
# Bug discovered during feature work
br create "Edge case causes crash" \
  -t bug \
  -p 1 \
  --deps discovered-from:bd-123 \
  --json

# Subtask for epic
br create "Implement auth middleware" \
  -t task \
  --parent bd-epic-456 \
  --json
```

### Closing with Suggestions

```bash
# Close and get next unblocked work
br close bd-123 --suggest-next --json
```

Returns:
```json
{
  "closed": "bd-123",
  "unblocked": ["bd-456", "bd-789"]
}
```

---

## Parsing JSON Output

### Python Example

```python
import subprocess
import json

def br_command(*args):
    """Run br command and return parsed JSON."""
    result = subprocess.run(
        ['br', *args, '--json'],
        capture_output=True,
        text=True
    )
    if result.returncode != 0:
        error = json.loads(result.stdout)
        raise RuntimeError(f"br error: {error.get('message', 'Unknown')}")
    return json.loads(result.stdout)

# Find ready work
ready = br_command('ready', '--limit', '5')
for issue in ready:
    print(f"{issue['id']}: {issue['title']}")

# Claim first issue
if ready:
    br_command('update', ready[0]['id'], '--claim')
```

### JavaScript/Node Example

```javascript
const { execSync } = require('child_process');

function br(...args) {
  const result = execSync(`br ${args.join(' ')} --json`, {
    encoding: 'utf-8',
    stdio: ['pipe', 'pipe', 'pipe']
  });
  return JSON.parse(result);
}

// Find ready work
const ready = br('ready', '--limit', '5');
console.log(`Found ${ready.length} ready issues`);

// Claim and work
if (ready.length > 0) {
  br('update', ready[0].id, '--claim');
}
```

### jq Examples

```bash
# Get IDs of all ready issues
br ready --json | jq -r '.[].id'

# Get high-priority bugs
br list --json -t bug -p 0 -p 1 | jq '.[] | "\(.id): \(.title)"'

# Count by status
br list --json -a | jq 'group_by(.status) | map({status: .[0].status, count: length})'

# Find my assigned work
br list --json --assignee $(whoami) | jq '.[].title'
```

---

## Error Handling

### Exit Codes

| Code | Category | Example |
|------|----------|---------|
| 0 | Success | Command completed |
| 1 | Internal | Unexpected error |
| 2 | Database | Not initialized |
| 3 | Issue | Issue not found |
| 4 | Validation | Invalid priority value |
| 5 | Dependency | Cycle detected |
| 6 | Sync/JSONL | Parse error |
| 7 | Config | Missing config |
| 8 | I/O | File not found |

### Structured Error Response

```json
{
  "error_code": 3,
  "message": "Issue not found: bd-xyz999",
  "kind": "not_found",
  "recovery_hints": [
    "Check the issue ID spelling",
    "Use 'br list' to find valid IDs"
  ]
}
```

### Error Recovery Patterns

```python
def safe_close(issue_id, reason):
    """Close with retry on transient errors."""
    for attempt in range(3):
        try:
            return br_command('close', issue_id, '-r', reason)
        except RuntimeError as e:
            if 'database locked' in str(e) and attempt < 2:
                time.sleep(0.5)
                continue
            raise
```

---

## Robot Mode Flags

These flags enable machine-friendly output:

| Flag | Description |
|------|-------------|
| `--json` | JSON output for all data |
| `--robot` | Alias for `--json` |
| `--silent` | Output only essential data (e.g., just ID for create) |
| `--quiet` | Suppress non-error output |
| `--no-color` | Disable ANSI colors |

### Combining Flags

```bash
# Machine-friendly create
br create "New issue" --silent
# Output: bd-abc123

# Quiet mode with JSON
br close bd-123 --quiet --json
# Outputs JSON, no status messages
```

---

## Agent-Specific Configuration

### Claude Code / Anthropic Agents

```bash
# Set actor for audit trail
export BD_ACTOR="claude-agent"

# Workflow
br ready --json --limit 10
br update <id> --claim
# ... work ...
br close <id> --reason "Completed by Claude"
br sync --flush-only
```

### Cursor AI

```bash
# Initialize in project
br init --prefix cursor

# Use with Cursor's tool system
br ready --json
br show <id> --json
```

### Aider

```bash
# Aider integration
export BD_ACTOR="aider-$(date +%Y%m%d)"

# Check work before session
br ready --json | head -5
```

### GitHub Copilot Workspace

```bash
# Copilot-friendly workflow
br ready --json --assignee copilot
br update <id> --status in_progress --assignee copilot
```

---

## Best Practices

### DO

1. **Always use `--json`** for programmatic access
2. **Check exit codes** before parsing output
3. **Set `BD_ACTOR`** for audit trail attribution
4. **Use `--claim`** for atomic status+assignee updates
5. **Create discovered issues** with `--deps discovered-from:<id>`
6. **Sync at session end** with `br sync --flush-only`
7. **Use `br ready`** to find actionable work
8. **Include reasons** when closing issues

### DON'T

1. **Don't parse human output** - use `--json` instead
2. **Don't edit JSONL directly** - always use br commands
3. **Don't skip sync** - other agents need your changes
4. **Don't hold issues indefinitely** - close or unassign if stuck
5. **Don't create duplicate issues** - search first
6. **Don't ignore errors** - check exit codes and error messages

### Session Management

```bash
# Session start
br ready --json > /tmp/session_start.json

# Session end checklist
br sync --flush-only
git add .beads/
git commit -m "Update issues"
```

### Concurrent Agent Safety

```bash
# Use lock timeout for busy databases
br list --json --lock-timeout 5000

# Check for stale data
br sync --status --json
```

---

## Integration with bv (beads_viewer)

For advanced analysis, use `bv` robot commands:

```bash
# Priority analysis
bv --robot-priority | jq '.recommendations[0]'

# Dependency insights
bv --robot-insights | jq '.Bottlenecks'

# Execution plan
bv --robot-plan | jq '.parallel_groups'
```

See [AGENTS.md](../AGENTS.md) for detailed bv integration.

---

## Troubleshooting

### Common Issues

**"Database not initialized"**
```bash
br init --prefix myproj
```

**"Issue not found"**
```bash
# Use partial ID matching
br show abc  # Matches bd-abc123

# List to find correct ID
br list --json | jq '.[].id'
```

**"Database locked"**
```bash
# Increase lock timeout
br list --json --lock-timeout 10000
```

**"Cycle detected"**
```bash
# Check for cycles
br dep cycles --json

# Remove problematic dependency
br dep remove bd-123 bd-456
```

### Debug Logging

```bash
# Enable debug output
RUST_LOG=debug br ready --json 2>debug.log

# Verbose mode
br sync --flush-only -vv
```

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Complete command reference
- [AGENTS.md](../AGENTS.md) - Agent development guidelines
- [README.md](../README.md) - Project overview
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
