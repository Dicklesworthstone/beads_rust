# br CLI Reference

Comprehensive reference for all `br` (beads_rust) commands.

---

## Table of Contents

- [Global Options](#global-options)
- [Core Commands](#core-commands)
  - [init](#init)
  - [create](#create)
  - [q (quick capture)](#q-quick-capture)
  - [list](#list)
  - [show](#show)
  - [update](#update)
  - [close](#close)
  - [reopen](#reopen)
  - [delete](#delete)
- [Query Commands](#query-commands)
  - [blocked](#blocked)
  - [search](#search)
  - [count](#count)
  - [stale](#stale)
- [Organization Commands](#organization-commands)
  - [dep](#dep)
  - [label](#label)
  - [epic](#epic)
  - [comments](#comments)
- [Workflow Commands](#workflow-commands)
  - [defer / undefer](#defer--undefer)
  - [orphans](#orphans)
  - [query (saved queries)](#query-saved-queries)
- [Sync & Config](#sync--config)
  - [sync](#sync)
  - [config](#config)
- [Diagnostics & Info](#diagnostics--info)
  - [stats / status](#stats--status)
  - [doctor](#doctor)
  - [version](#version)
  - [audit](#audit)
  - [history](#history)
  - [changelog](#changelog)
  - [lint](#lint)
- [Utilities](#utilities)
  - [upgrade](#upgrade)
  - [completions](#completions)
- [Exit Codes](#exit-codes)
- [Environment Variables](#environment-variables)
- [JSON Output Schemas](#json-output-schemas)

---

## Global Options

These options apply to all commands:

| Option | Description |
|--------|-------------|
| `--db <PATH>` | Database path (auto-discover `.beads/*.db` if not set) |
| `--actor <NAME>` | Actor name for audit trail |
| `--json` | Output as JSON (machine-readable) |
| `--no-daemon` | Force direct mode (no daemon) |
| `--no-auto-flush` | Skip automatic JSONL export after mutations |
| `--no-auto-import` | Skip automatic import check |
| `--allow-stale` | Allow stale DB (bypass freshness check warning) |
| `--lock-timeout <MS>` | SQLite busy timeout in milliseconds |
| `--no-db` | JSONL-only mode (no DB connection) |
| `-v, --verbose` | Increase logging verbosity (-v, -vv) |
| `-q, --quiet` | Quiet mode (errors only) |
| `--no-color` | Disable colored output |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

---

## Core Commands

### init

Initialize a beads workspace in the current directory.

```bash
br init [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--force` | Overwrite existing database |

`init` no longer accepts `--prefix`. Issue prefixes are always explicit at
creation time (`br create --prefix`, `br q --prefix`) — there is no
project-wide default prefix.

**Examples:**
```bash
# Initialize a workspace
br init

# Force reinitialize
br init --force
```

---

### create

Create a new issue.

```bash
br create [OPTIONS] [TITLE]
```

**Arguments:**
- `TITLE` - Issue title (can also use `--title-flag`)

**Options:**
| Option | Description |
|--------|-------------|
| `--prefix <PREFIX>` | **Required.** Issue ID prefix for the new bead (e.g. `demo`). No default — there is no config file, DB row, or `BD_ISSUE_PREFIX` env var fallback. |
| `-t, --type <TYPE>` | Issue type (task, bug, feature, epic, chore, docs, question) |
| `-p, --priority <PRIORITY>` | Priority (0-4 or P0-P4, where 0=critical) |
| `-d, --description <TEXT>` | Issue description |
| `-a, --assignee <NAME>` | Assign to person |
| `--owner <EMAIL>` | Set owner email |
| `-l, --labels <LABELS>` | Labels (comma-separated) |
| `--parent <ID>` | Parent issue ID (creates parent-child dependency) |
| `--deps <DEPS>` | Dependencies (format: `type:id,type:id`) |
| `-e, --estimate <MINUTES>` | Time estimate in minutes |
| `--due <DATE>` | Due date (RFC3339 or relative like `+2d`, `tomorrow`) |
| `--defer <DATE>` | Defer until date |
| `--external-ref <REF>` | External reference (e.g., `gh-123`) |
| `--ephemeral` | Mark as ephemeral (not exported to JSONL) |
| `--dry-run` | Preview without creating |
| `--silent` | Output only issue ID |
| `-f, --file <PATH>` | Create issues from markdown file (bulk import) |

**Examples:**
```bash
# Simple task
br create "Fix login bug" --prefix myproj

# High-priority bug with details
br create "Critical security issue" --prefix myproj -t bug -p 0 -d "XSS vulnerability in form input"

# Feature with assignee and labels
br create "Add dark mode" --prefix myproj -t feature -a alice -l "ui,enhancement"

# Task with due date
br create "Deploy to production" --prefix myproj --due "+3d"

# Bulk import from markdown
br create -f issues.md --prefix myproj
```

---

### q (quick capture)

Quick capture - create issue and print only the ID.

```bash
br q [OPTIONS] --prefix <PREFIX> <TITLE>
```

Same options as `create`, but outputs only the issue ID for scripting.
`--prefix` is required here too — there is no default.

**Example:**
```bash
# Capture and immediately assign
ISSUE=$(br q "Quick fix needed" --prefix myproj)
br update $ISSUE --assignee me
```

---

### list

List issues with filtering and sorting.

```bash
br list [OPTIONS]
```

**Filter Options:**
| Option | Description |
|--------|-------------|
| `-s, --status <STATUS>` | Filter by status (can repeat) |
| `-t, --type <TYPE>` | Filter by issue type (can repeat) |
| `--assignee <NAME>` | Filter by assignee |
| `--unassigned` | Show only unassigned issues |
| `--id <ID>` | Filter by specific IDs (can repeat) |
| `-l, --label <LABEL>` | Filter by label (AND logic, can repeat) |
| `--label-any <LABEL>` | Filter by label (OR logic, can repeat) |
| `-p, --priority <PRIORITY>` | Filter by priority (can repeat) |
| `--priority-min <N>` | Filter by minimum priority |
| `--priority-max <N>` | Filter by maximum priority |
| `--title-contains <TEXT>` | Title contains substring |
| `--desc-contains <TEXT>` | Description contains substring |
| `-a, --all` | Include closed issues |
| `--deferred` | Include deferred issues |
| `--overdue` | Filter for overdue issues |

**Output Options:**
| Option | Description |
|--------|-------------|
| `--limit <N>` | Maximum results (0=unlimited, default: 50) |
| `--sort <FIELD>` | Sort by: priority, created_at, updated_at, title |
| `-r, --reverse` | Reverse sort order |
| `--long` | Long output format |
| `--pretty` | Tree/pretty output format |
| `--format <FMT>` | Output format: text, json, csv |
| `--fields <FIELDS>` | CSV fields (comma-separated) |

**Examples:**
```bash
# All open issues
br list

# High-priority bugs
br list -t bug -p 0 -p 1

# My assigned work
br list --assignee $(whoami)

# Export to CSV
br list --format csv --fields id,title,status,priority > issues.csv

# JSON for scripting
br list --json | jq '.[].id'
```

---

### show

Show detailed issue information.

```bash
br show [IDS]...
```

**Examples:**
```bash
# Show single issue
br show bd-abc123

# Show multiple issues
br show bd-abc123 bd-def456

# JSON output
br show bd-abc123 --json
```

---

### update

Update one or more issues.

```bash
br update [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--title <TEXT>` | Update title (REPLACES the field) |
| `--description <TEXT>` | Update description (REPLACES the field) |
| `--design <TEXT>` | Update design notes (REPLACES the field) |
| `--acceptance-criteria <TEXT>` | Update acceptance criteria (REPLACES the field) |
| `--notes <TEXT>` | Update additional notes (REPLACES the field) |
| `--replace` | Allow a write that shrinks a non-empty free-text field |
| `-s, --status <STATUS>` | Change status |
| `-p, --priority <N>` | Change priority |
| `-t, --type <TYPE>` | Change issue type |
| `--assignee <NAME>` | Assign (empty string clears) |
| `--owner <EMAIL>` | Set owner (empty string clears) |
| `--claim` | Atomic claim (assignee=actor + status=in_progress) |
| `--due <DATE>` | Set due date (empty string clears) |
| `--defer <DATE>` | Set defer date (empty string clears) |
| `--estimate <MINUTES>` | Set time estimate |
| `--add-label <LABEL>` | Add label(s) |
| `--remove-label <LABEL>` | Remove label(s) |
| `--set-labels <LABELS>` | Replace all labels |
| `--parent <ID>` | Reparent (empty string removes) |
| `--external-ref <REF>` | Set external reference |

**Examples:**
```bash
# Claim a task
br update bd-abc123 --claim

# Change status
br update bd-abc123 -s in_progress

# Update multiple issues
br update bd-abc123 bd-def456 -p 1

# Add labels
br update bd-abc123 --add-label "urgent,reviewed"
```

**Free-text fields replace, and shrinking one is refused**

`--title`, `--description`, `--design`, `--acceptance-criteria` and `--notes`
each replace the whole field. A write that would shrink one of those fields
while it has content is REFUSED — nothing is written, and the command exits 4:

```bash
$ br update bd-abc123 --notes "second block"
{
  "error": {
    "code": "DESTRUCTIVE_UPDATE",
    "message": "Refusing destructive update: notes on bd-abc123 would shrink from 63 to 12 chars",
    ...
  }
}
```

Three ways forward:

- to ADD to the field, use `br comments add bd-abc123 -f <file>` — append-only,
  attributed and timestamped;
- to rewrite it while keeping what is there, re-send the flag with the existing
  text included;
- to replace it anyway, pass `--replace`.

Every allowed write to those fields reports its size delta, so an append and an
annihilation no longer look the same:

```bash
$ br update bd-abc123 --notes "$(cat note.md)"
Updated bd-abc123: fix the widget  (notes: 3535 → 4210 chars, prior content retained)
```

`prior content retained` means the previous value still appears verbatim in the
new one; `PRIOR CONTENT NOT RETAINED` means it does not — a growing write can
still drop everything that was there. Under `--json` the same figures appear as
a `text_deltas` array on each updated issue.

Identifier-like fields (`--external-ref`, `--session`, `--assignee`, `--owner`,
`--due`, `--defer`) are NOT guarded: they are replaced by nature, several
document "empty string clears", and a shorter value there is an ordinary
correction rather than a loss.

---

### close

Close one or more issues.

```bash
br close [OPTIONS] [IDS]...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Close reason |
| `-f, --force` | Close even if blocked by open dependencies |
| `--suggest-next` | Return newly unblocked issues |
| `--session <ID>` | Session ID for tracking |
| `--robot` | Machine-readable output |

**Examples:**
```bash
# Close with reason
br close bd-abc123 -r "Completed in PR #42"

# Close multiple
br close bd-abc123 bd-def456 -r "Sprint complete"

# Force close blocked issue
br close bd-abc123 --force

# Close and get next work
br close bd-abc123 --suggest-next --json
```

---

### reopen

Reopen a closed issue.

```bash
br reopen <IDS>...
```

---

### delete

Delete an issue (creates tombstone).

```bash
br delete [OPTIONS] <IDS>...
```

**Options:**
| Option | Description |
|--------|-------------|
| `-r, --reason <TEXT>` | Deletion reason |
| `-f, --force` | Skip confirmation |

---

## Query Commands

### blocked

List blocked issues.

```bash
br blocked [OPTIONS]
```

Shows issues that are blocked by other open issues.

---

### search

Full-text search across issues.

```bash
br search <QUERY> [OPTIONS]
```

Supports all filter options from `list`.

**Examples:**
```bash
# Search in all fields
br search "authentication"

# Search with filters
br search "bug" -t bug --assignee alice
```

---

### count

Count issues with optional grouping.

```bash
br count [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--by <FIELD>` | Group by: status, type, priority, assignee, label |

**Examples:**
```bash
# Total count
br count

# Count by status
br count --by status

# Count by assignee
br count --by assignee --json
```

---

### stale

List stale issues (not updated recently).

```bash
br stale [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--days <N>` | Issues not updated in N days (default: 14) |

---

## Organization Commands

### dep

Manage dependencies between issues.

```bash
br dep <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ISSUE> <DEPENDS_ON>` | Add dependency (ISSUE depends on DEPENDS_ON) |
| `remove <ISSUE> <DEPENDS_ON>` | Remove dependency |
| `list <ISSUE>` | List dependencies of an issue |
| `tree <ISSUE>` | Show dependency tree |
| `cycles` | Detect dependency cycles |

**Dependency Types:**
- `blocks` (default) - Target blocks source
- `parent-child` - Hierarchical relationship
- `discovered-from` - Discovered during work on another issue
- `related` - Loosely related issues

**Examples:**
```bash
# Add blocking dependency
br dep add bd-123 bd-456  # bd-123 is blocked by bd-456

# Add with type
br dep add bd-123 bd-456 --type discovered-from

# Show tree
br dep tree bd-123

# Check for cycles
br dep cycles
```

---

### label

Manage labels on issues.

```bash
br label <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ID> <LABELS>` | Add labels to issue |
| `remove <ID> <LABELS>` | Remove labels from issue |
| `list [ID]` | List labels (optionally for specific issue) |

---

### epic

Epic management commands.

```bash
br epic <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `status <ID>` | Show epic status with child progress |
| `close-eligible <ID>` | Check if epic can be closed |

---

### comments

Manage comments on issues.

```bash
br comments <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `add <ID> <BODY>` | Add comment |
| `list <ID>` | List comments |

---

## Workflow Commands

### defer / undefer

Defer or undefer issues.

```bash
br defer <IDS>... [OPTIONS]
br undefer <IDS>...
```

**Options:**
| Option | Description |
|--------|-------------|
| `--until <DATE>` | Defer until date |

---

### orphans

List orphan issues (referenced in commits but still open).

```bash
br orphans [OPTIONS]
```

---

### query (saved queries)

Manage saved queries.

```bash
br query <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `save <NAME> <QUERY>` | Save a query |
| `run <NAME>` | Run a saved query |
| `list` | List saved queries |
| `delete <NAME>` | Delete a saved query |

---

## Sync & Config

### sync

Sync database with JSONL file.

```bash
br sync [OPTIONS]
```

**SAFETY GUARANTEES:**
- NEVER executes git commands or auto-commits
- NEVER modifies files outside `.beads/` (unless `--allow-external-jsonl`)
- Uses atomic temp-file-then-rename pattern
- Safety guards prevent accidental data loss

**Modes (one required unless --status):**
| Option | Description |
|--------|-------------|
| `--flush-only` | Export database to JSONL |
| `--import-only` | Import JSONL into database |
| `--status` | Show sync status (read-only) |

**Options:**
| Option | Description |
|--------|-------------|
| `-f, --force` | Override safety guards (use with caution) |
| `--allow-external-jsonl` | Allow JSONL path outside `.beads/` |
| `--manifest` | Write manifest file with export summary |
| `--error-policy <POLICY>` | Export error handling: strict, best-effort, partial, required-core |
| `--orphans <MODE>` | Orphan handling: strict, resurrect, skip, allow |
| `--robot` | Machine-readable output |

**Examples:**
```bash
# Export to JSONL
br sync --flush-only

# Import from JSONL
br sync --import-only

# Check sync status
br sync --status

# Export with verbose logging
br sync --flush-only -v
```

---

### config

Configuration management.

```bash
br config [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `-l, --list` | List all config options with descriptions |
| `-g, --get <KEY>` | Get a specific config value |
| `-s, --set <KEY=VALUE>` | Set a config value |
| `-d, --delete <KEY>` | Delete a config value |
| `-e, --edit` | Open config in `$EDITOR` |
| `-p, --path` | Show config file paths |
| `--project` | Show only project config |
| `--user` | Show only user config |

**Examples:**
```bash
# List all config
br config --list

# Get specific value
br config --get id.prefix

# Set value
br config --set id.prefix=myproj

# Edit in editor
br config --edit
```

---

## Diagnostics & Info

### stats / status

Show project statistics.

```bash
br stats
br status  # alias
```

---

### doctor

Run read-only diagnostics.

```bash
br doctor
```

Checks database integrity, schema compatibility, and configuration.

---

### info

Show workspace diagnostics and metadata.

```bash
br info [--schema] [--whats-new] [--thanks]
```

---

### where

Show the active `.beads` directory (after redirects, if any).

```bash
br where
```

---

### version

Show version information.

```bash
br version
```

---

### audit

Record and label agent interactions.

```bash
br audit [OPTIONS]
```

Appends to `.beads/interactions.jsonl`.

---

### history

Manage local history backups.

```bash
br history <COMMAND>
```

**Subcommands:**
| Command | Description |
|---------|-------------|
| `list` | List backups |
| `restore <BACKUP>` | Restore from backup |

**Notes:**
- Backups are created during `br sync --flush-only` when overwriting a JSONL file inside `.beads/`, including custom `BEADS_JSONL` paths that still target `.beads/`.

---

### changelog

Generate changelog from closed issues.

```bash
br changelog [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--since <DATE>` | Include issues closed since date |
| `--format <FMT>` | Output format: markdown, json |

---

### lint

Check issues for missing template sections.

```bash
br lint [OPTIONS]
```

---

## Utilities

### upgrade

Upgrade br to the latest version.

```bash
br upgrade [OPTIONS]
```

**Options:**
| Option | Description |
|--------|-------------|
| `--check` | Check for updates without installing |
| `--force` | Force reinstall current version |

---

### completions

Generate shell completions.

```bash
br completions <SHELL>
```

**Shells:** bash, zsh, fish, powershell

**Example:**
```bash
# Add to ~/.bashrc
br completions bash >> ~/.bashrc
source ~/.bashrc
```

---

## Exit Codes

| Code | Category | Description |
|------|----------|-------------|
| 0 | Success | Command completed successfully |
| 1 | Internal | Internal error |
| 2 | Database | Database error (not initialized, schema mismatch) |
| 3 | Issue | Issue error (not found, ambiguous ID) |
| 4 | Validation | Validation error (invalid input) |
| 5 | Dependency | Dependency error (cycle detected, self-dependency) |
| 6 | Sync/JSONL | Sync error (parse error, conflict markers) |
| 7 | Config | Configuration error |
| 8 | I/O | I/O error (file not found, permission denied) |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `BEADS_DIR` | Override `.beads` directory location |
| `BEADS_JSONL` | Override JSONL file path (requires `--allow-external-jsonl`) |
| `BD_ACTOR` | Default actor name for audit trail |
| `EDITOR` | Editor for `br config --edit` |
| `NO_COLOR` | Disable colored output (any value) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

---

## JSON Output Schemas

### Issue Object (list, show, blocked)

```json
{
  "id": "bd-abc123",
  "title": "Issue title",
  "description": "Full description text",
  "design": "",
  "acceptance_criteria": "",
  "notes": "",
  "status": "open",
  "priority": 2,
  "issue_type": "task",
  "assignee": "alice",
  "owner": "owner@example.com",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "bob",
  "updated_at": "2025-01-16T14:20:00Z",
  "close_reason": "",
  "closed_by_session": "",
  "source_system": "",
  "deleted_by": "",
  "delete_reason": "",
  "sender": "",
  "dependency_count": 0,
  "dependent_count": 3
}
```

### Dependency Object

```json
{
  "issue_id": "bd-abc123",
  "depends_on_id": "bd-def456",
  "dep_type": "blocks",
  "created_at": "2025-01-15T10:30:00Z",
  "created_by": "alice"
}
```

### Sync Status Object

```json
{
  "db_path": ".beads/beads.db",
  "jsonl_path": ".beads/issues.jsonl",
  "db_modified": "2025-01-16T14:20:00Z",
  "jsonl_modified": "2025-01-16T14:15:00Z",
  "db_issue_count": 150,
  "jsonl_issue_count": 148,
  "dirty_count": 2,
  "status": "db_newer"
}
```

### Error Object

```json
{
  "error_code": 3,
  "message": "Issue not found: bd-xyz999",
  "kind": "not_found",
  "recovery_hints": ["Check the issue ID", "Use 'br list' to find issues"]
}
```

---

## See Also

- [README.md](../README.md) - Project overview
- [AGENTS.md](../AGENTS.md) - Agent integration guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
