# br Configuration

## Configuration Precedence (High to Low)

1. **CLI flags** (highest priority)
2. **Environment variables**
3. **Project config**: `.beads/config.yaml`
4. **User config**: `~/.config/beads/config.yaml`
5. **Defaults** (lowest priority)

---

## Example Config File

```yaml
# .beads/config.yaml

# Issue ID prefix (default: "bd")
id:
  prefix: "myproject"

# Default values for new issues
defaults:
  priority: 2        # P2 = MEDIUM (0-4 scale)
  type: "task"
  assignee: "team@example.com"

# Output formatting
output:
  color: true
  date_format: "%Y-%m-%d"

# Sync behavior
sync:
  auto_import: false
  auto_flush: false
  branch: beads-sync  # Use dedicated sync branch
```

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `USER` | Fallback actor identity for the audit trail, used when neither `--actor` nor the `actor` config key is set. `BR_ACTOR` is only a shell convention in this skill's snippets, which pass it through as `--actor`; `br` itself does not read it. |
| `BEADS_DIR` | Use this `.beads` directory instead of discovering one from the working directory |
| `BEADS_DB` (aliases: `BD_DB`, `BD_DATABASE`) | Override database path |
| `BEADS_JSONL` | Override JSONL path (requires `--allow-external-jsonl`) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

---

## Config Commands

```bash
br config list                       # Show all config
br config get id.prefix              # Get specific value
br config set defaults.priority=1    # Set value
```

---

## Storage Paths

Default storage is in `.beads/` relative to project root:

```
.beads/
  beads.db        # SQLite database (primary storage)
  beads.db-shm    # SQLite shared memory (WAL mode)
  beads.db-wal    # SQLite write-ahead log
  issues.jsonl    # JSONL export (for git)
  config.yaml     # Project configuration
  metadata.json   # Workspace metadata
```
