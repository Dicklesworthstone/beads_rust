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
4. **Use `br list --status open`** to find actionable work
5. **Sync at session end** with `br sync --flush-only`

---

## Quick Start for Agents

```bash
# Initialize (if needed) — issue prefixes are no longer set at init time
br init

# Find work
br list --status open --json --limit 5

# Claim and work
br update bd-123 --claim --json
# ... do the work ...
br close bd-123 --reason "Implemented feature X" --json

# Create discovered work (--prefix is mandatory for every creation command)
br create "Found bug during implementation" --prefix myproj -t bug -p 1 --deps discovered-from:bd-123 --json

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
br create "Title" --prefix myproj --json

# Equivalent (when the command supports --format)
br list --format json

# Robot mode alias (same as --json)
br list --robot
br close bd-123 --robot
```

### TOON Output (Token-Efficient)

Many read-style commands support TOON output via `--format toon`:

```bash
br list --format toon --limit 10
br show bd-123 --format toon
```

Decode TOON to JSON when you need to pipe into JSON tools:

```bash
br list --format toon --limit 10 | tru --decode | jq '.[0]'
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
- **Arrays for lists** - `br list`, `br blocked`, `br search`
- **Objects for single items** - `br show`, `br create`
- **Structured errors** - error object with code and hints

### Example Output

```bash
$ br list --status open --json --limit 2
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
│     br list --status open --json                            │
│     → Find open issues to work on                           │
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
│       br create "New issue" --prefix myproj \                │
│         --deps discovered-from:<id>                         │
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
# Bug discovered during feature work (--prefix is mandatory)
br create "Edge case causes crash" \
  --prefix myproj \
  -t bug \
  -p 1 \
  --deps discovered-from:bd-123 \
  --json

# Subtask for epic
br create "Implement auth middleware" \
  --prefix myproj \
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

# Find open work
open_issues = br_command('list', '--status', 'open', '--limit', '5')
for issue in open_issues:
    print(f"{issue['id']}: {issue['title']}")

# Claim first issue
if open_issues:
    br_command('update', open_issues[0]['id'], '--claim')
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

// Find open work
const openIssues = br('list', '--status', 'open', '--limit', '5');
console.log(`Found ${openIssues.length} open issues`);

// Claim and work
if (openIssues.length > 0) {
  br('update', openIssues[0].id, '--claim');
}
```

### jq Examples

```bash
# Get IDs of all open issues
br list --status open --json | jq -r '.[].id'

# Get high-priority bugs
br list --json -t bug -p 0 -p 1 | jq '.[] | "\(.id): \(.title)"'

# Count by status
br list --json -a | jq 'group_by(.status) | map({status: .[0].status, count: length})'

# Find my assigned work
br list --json --assignee $(whoami) | jq '.[].title'
```

---

## Error Handling

### Writing to free-text fields

`--title`, `--description`, `--design`, `--acceptance-criteria` and `--notes`
REPLACE the whole field. To accumulate narrative, use `br comments add <id> -f
<file>` — it is append-only, attributed and timestamped.

A write that would shrink one of those fields while it has content is refused
(`DESTRUCTIVE_UPDATE`, exit 4) and nothing is written; pass `--replace` if you
genuinely mean to discard what is there. Allowed writes report the size change
on the success line, and under `--json` as `text_deltas`:

```json
"text_deltas": [
  { "field": "notes", "old_chars": 3535, "new_chars": 4210, "prior_content_retained": true }
]
```

Two things worth checking programmatically:

- `prior_content_retained: false` on a growing write means the previous value
  is no longer present — typically a read-modify-write whose read failed;
- `landed_as_sent: false` (present only when something is wrong, exit 2,
  `WRITE_MISMATCH`) means what is stored is NOT what bd was handed. Do not
  treat such an update as applied.

Do not verify a write by grepping for the text you just sent: that succeeds on
a field you have just destroyed. Compare the whole field, and treat a length
decrease as failure.

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
br create "New issue" --prefix myproj --silent
# Output: myproj-abc123

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
br list --status open --json --limit 10
br update <id> --claim
# ... work ...
br close <id> --reason "Completed by Claude"
br sync --flush-only
```

### Cursor AI

```bash
# Initialize in project (no --prefix on init; set it per-create instead)
br init

# Use with Cursor's tool system
br list --status open --json
br create "New task" --prefix cursor --json
br show <id> --json
```

### Aider

```bash
# Aider integration
export BD_ACTOR="aider-$(date +%Y%m%d)"

# Check work before session
br list --status open --json | head -5
```

### GitHub Copilot Workspace

```bash
# Copilot-friendly workflow
br list --status open --json --assignee copilot
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
7. **Use `br list --status open`** to find actionable work
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
br list --status open --json > /tmp/session_start.json

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
br init
```

**"--prefix is required for issue creation"**
```bash
# Every creation command (br create, br q) requires an explicit --prefix.
# There is no config file, DB row, or BD_ISSUE_PREFIX env var fallback.
br create "My issue" --prefix myproj
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
RUST_LOG=debug br list --status open --json 2>debug.log

# Verbose mode
br sync --flush-only -vv
```

---

## Migration from `BD_ISSUE_PREFIX`

Older harnesses and skills exported `BD_ISSUE_PREFIX` to set a default
issue prefix for creation and to scope `bd list`/`bd ready` output. Both
uses are gone:

- **Creation is always explicit.** `br create` and `br q` require
  `--prefix <name>` on every invocation. There is no config file, DB row,
  YAML key, or environment variable that supplies a default prefix.
  `BD_ISSUE_PREFIX` is read by nothing and has zero effect.
- **Identity uses `BD_AGENT_ID` instead.** Messaging (`br msg`/`br
  inbox`/`br outbox`), `br watch`, and presence (`br working`/`br idle`)
  resolve the calling agent's identity from `BD_AGENT_ID`, not
  `BD_ISSUE_PREFIX`. Update harness/skill env exports accordingly:
  `BD_AGENT_ID=myagent` instead of `BD_ISSUE_PREFIX=myagent`.
- **`br list` no longer self-scopes by identity.** Default output shows
  all prefixes; use `--prefix <name>` to filter explicitly.

If your harness still sets `BD_ISSUE_PREFIX`, it is harmless (ignored)
but you should migrate to `BD_AGENT_ID` and add explicit `--prefix` flags
to any scripted `br create`/`br q` calls.

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Complete command reference
- [AGENTS.md](../AGENTS.md) - Agent development guidelines
- [README.md](../README.md) - Project overview
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
