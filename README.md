# br - Beads Rust

<div align="center">
  <img src="br_illustration.webp" alt="br - Fast, non-invasive issue tracker for git repositories" width="600">
</div>

<div align="center">

[![CI](https://github.com/Dicklesworthstone/beads_rust/actions/workflows/ci.yml/badge.svg)](https://github.com/Dicklesworthstone/beads_rust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/storage-SQLite-green.svg)](https://www.sqlite.org/)

</div>

A Rust port of Steve Yegge's [beads](https://github.com/steveyegge/beads), frozen at the "classic" SQLite + JSONL architecture I built my Agent Flywheel tooling around.

[Quick Start](#quick-start) | [Commands](#commands) | [Configuration](#configuration) | [FAQ](#faq)

<div align="center">
<h3>Quick Install</h3>

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
```

<p><em>Works on Linux, macOS, and Windows (WSL). Auto-detects your platform and downloads the right binary.</em></p>
</div>

---

## Why This Project Exists

I (Jeffrey Emanuel) LOVE [Steve Yegge's Beads project](https://github.com/steveyegge/beads). Discovering it and seeing how well it worked together with my [MCP Agent Mail](https://github.com/Dicklesworthstone/mcp-agent-mail) was a truly transformative moment in my development workflows and professional life. This quickly also led to [beads_viewer (bv)](https://github.com/Dicklesworthstone/beads_viewer), which added another layer of analysis to beads that gives swarms of agents the insight into what beads they should work on next to de-bottleneck the development process and increase velocity. I'm very grateful for finding beads when I did and to Steve for making it.

At this point, my [Agent Flywheel](http://agent-flywheel.com/tldr) System is built around beads operating in a specific way. As Steve continues evolving beads toward [GasTown](https://github.com/steveyegge/gastown) and beyond, our use cases have naturally diverged. The hybrid SQLite + JSONL-git architecture that I built my tooling around (and independently mirrored in MCP Agent Mail) is being replaced with approaches better suited to Steve's vision.

Rather than ask Steve to maintain a legacy mode for my niche use case, I created this Rust port that freezes the "classic beads" architecture I depend on. The command is `br` to distinguish it from the original `bd`.

**This isn't a criticism of beads**; Steve's taking it in exciting directions. It's simply that my tooling needs a stable snapshot of the architecture I built around, and maintaining my own fork is the right solution for that. Steve has given his full endorsement of this project.

---

## TL;DR

### The Problem

You need to track issues for your project, but:
- **GitHub/GitLab Issues** require internet, fragment context from code, and don't work offline
- **TODO comments** get lost, have no status tracking, and can't express dependencies
- **External tools** (Jira, Linear) add overhead, require context switching, and cost money

### The Solution

**br** is a local-first issue tracker that stores issues in SQLite with JSONL export for git-friendly collaboration. It's **20K lines of Rust** focused on one thing: tracking issues without getting in your way.

```bash
br init                              # Initialize in your repo
br create "Fix login timeout" -p 1   # Create high-priority issue
br ready                             # See what's actionable
br close bd-abc123                   # Close when done
br sync --flush-only                 # Export for git commit
```

### Why br?

| Feature | br | GitHub Issues | Jira | TODO comments |
|---------|-----|---------------|------|---------------|
| Works offline | **Yes** | No | No | Yes |
| Lives in repo | **Yes** | No | No | Yes |
| Tracks dependencies | **Yes** | Limited | Yes | No |
| Zero cost | **Yes** | Free tier | No | Yes |
| No account required | **Yes** | No | No | Yes |
| Machine-readable | **Yes** (`--json`) | API only | API only | No |
| Git-friendly sync | **Yes** (JSONL) | N/A | N/A | N/A |
| Non-invasive | **Yes** | N/A | N/A | Yes |
| AI agent integration | **Yes** | Limited | Limited | No |

---

## Quick Example

```bash
# Initialize br in your project
cd my-project
br init

# Add agent instructions to AGENTS.md (creates file if needed)
br agents --add --force

# Create issues with priority (0=critical, 4=backlog)
br create "Implement user auth" --type feature --priority 1
# Created: bd-7f3a2c

br create "Set up database schema" --type task --priority 1
# Created: bd-e9b1d4

# Auth depends on database schema
br dep add bd-7f3a2c bd-e9b1d4

# See what's ready to work on (not blocked)
br ready
# bd-e9b1d4  P1  task     Set up database schema

# Claim and complete work
br update bd-e9b1d4 --status in_progress
br close bd-e9b1d4 --reason "Schema implemented"

# Now auth is unblocked
br ready
# bd-7f3a2c  P1  feature  Implement user auth

# Export to JSONL for git commit
br sync --flush-only
git add .beads/ && git commit -m "Update issues"
```

---

## Design Philosophy

### 1. Non-Invasive by Default

br **never** touches your source code or runs git commands automatically. Other tools might auto-commit or install hooks without asking. br doesn't.

```bash
# br only touches .beads/ directory
ls -la .beads/
# beads.db       # SQLite database
# issues.jsonl   # Git-friendly export
# config.yaml    # Optional config
```

### 2. SQLite + JSONL Hybrid

**SQLite** for fast local queries. **JSONL** for git-friendly collaboration.

```bash
# Local: Fast queries via SQLite
br list --priority 0-1 --status open --assignee alice

# Collaboration: JSONL merges cleanly in git
git diff .beads/issues.jsonl
# +{"id":"bd-abc123","title":"New feature",...}
```

### 3. Explicit Over Implicit

Every operation is explicit. No magic, no surprises.

```bash
# Export is explicit (not automatic)
br sync --flush-only

# Import is explicit (not automatic)
br sync --import-only

# Git operations are YOUR responsibility
git add .beads/ && git commit -m "..."
```

### 4. Agent-First Design

Every command supports `--json` for AI coding agents:

```bash
br list --json | jq '.[] | select(.priority <= 1)'
br ready --json  # Structured output for agents
br show bd-abc123 --json
```

### 5. Rich Terminal Output

Interactive terminals get enhanced visual output:

```bash
# Rich mode (default in TTY)
br list           # Formatted tables with colors
br show bd-abc    # Styled panels with metadata

# Plain mode (piped or --no-color)
br list | cat     # Clean text, no ANSI codes

# JSON mode (--json or --robot)
br list --json    # Structured output for tools
```

Output mode is auto-detected:
- **Rich**: Interactive TTY with color support
- **Plain**: Piped output or `NO_COLOR` environment
- **JSON**: Machine-readable (`--json` flag)
- **Quiet**: Minimal output (`--quiet` flag)

### 6. Minimal Footprint

~20K lines of Rust vs ~276K lines in the original Go beads. Faster compilation, smaller binary, fewer moving parts.

---

## Comparison vs Alternatives

### br vs Original beads (Go)

| Aspect | br (Rust) | beads (Go) |
|--------|-----------|------------|
| Lines of code | ~20,000 | ~276,000 |
| Git operations | **Never** (explicit) | Auto-commit, hooks |
| Storage | SQLite + JSONL | Dolt/SQLite |
| Background daemon | **No** | Yes |
| Hook installation | **Manual** | Automatic |
| Binary size | ~5-8 MB | ~30+ MB |
| Complexity | Focused | Feature-rich |

**When to use br:** You want a stable, minimal issue tracker that stays out of your way.

**When to use beads:** You want advanced features like Linear/Jira sync, RPC daemon, automatic hooks.

### br vs GitHub Issues

| Aspect | br | GitHub Issues |
|--------|-----|---------------|
| Works offline | **Yes** | No |
| Lives in repo | **Yes** | Separate |
| Dependencies | **Yes** | Workarounds |
| Custom fields | Via labels | Limited |
| Machine API | `--json` flag | REST API |
| Cost | Free | Free (limits) |

### br vs Linear/Jira

| Aspect | br | Linear/Jira |
|--------|-----|-------------|
| Setup time | 1 command | Account + config |
| Cost | Free | $8-15/user/mo |
| Works offline | **Yes** | Limited |
| Learning curve | CLI | GUI + workflows |
| Git integration | Native | Webhooks |

---

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
```

### From Source

```bash
# Requires Rust nightly
git clone https://github.com/Dicklesworthstone/beads_rust.git
cd beads_rust
cargo build --release
./target/release/br --help

# Or install globally
cargo install --path .
```

### Cargo Install

```bash
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

### Disable Self-Update

```bash
# Build without self-update feature
cargo build --release --no-default-features

# Or install without it
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git --no-default-features
```

### Verify Installation

```bash
br --version
# br 0.1.0 (rustc 1.85.0-nightly)
```

---

## Quick Start

### 1. Initialize in Your Project

```bash
cd my-project
br init
# Initialized beads workspace in .beads/
```

### 2. Create Your First Issue

```bash
br create "Fix login timeout bug" \
  --type bug \
  --priority 1 \
  --description "Users report login times out after 30 seconds"
# Created: bd-a1b2c3
```

### 3. Add Labels

```bash
br label add bd-a1b2c3 backend auth
```

### 4. Check Ready Work

```bash
br ready
# Shows issues that are open, not blocked, not deferred
```

### 5. Claim and Work

```bash
br update bd-a1b2c3 --status in_progress --assignee "$(git config user.email)"
```

### 6. Close When Done

```bash
br close bd-a1b2c3 --reason "Increased timeout to 60s, added retry logic"
```

### 7. Sync to Git

```bash
br sync --flush-only        # Export DB to JSONL
git add .beads/             # Stage changes
git commit -m "Fix: login timeout (bd-a1b2c3)"
```

---

## Commands

### Issue Lifecycle

| Command | Description | Example |
|---------|-------------|---------|
| `init` | Initialize workspace | `br init` |
| `create` | Create issue | `br create "Title" -p 1 --type bug` |
| `q` | Quick capture (ID only) | `br q "Fix typo"` |
| `show` | Show issue details | `br show bd-abc123` |
| `update` | Update issue | `br update bd-abc123 --priority 0` |
| `close` | Close issue | `br close bd-abc123 --reason "Done"` |
| `reopen` | Reopen closed issue | `br reopen bd-abc123` |
| `delete` | Delete issue (tombstone) | `br delete bd-abc123` |

### Querying

| Command | Description | Example |
|---------|-------------|---------|
| `list` | List issues | `br list --status open --priority 0-1` |
| `ready` | Actionable work | `br ready` |
| `blocked` | Blocked issues | `br blocked` |
| `search` | Full-text search | `br search "authentication"` |
| `stale` | Stale issues | `br stale --days 30` |
| `count` | Count with grouping | `br count --by status` |

### Dependencies

| Command | Description | Example |
|---------|-------------|---------|
| `dep add` | Add dependency | `br dep add bd-child bd-parent` |
| `dep remove` | Remove dependency | `br dep remove bd-child bd-parent` |
| `dep list` | List dependencies | `br dep list bd-abc123` |
| `dep tree` | Dependency tree | `br dep tree bd-abc123` |
| `dep cycles` | Find cycles | `br dep cycles` |

### Labels

| Command | Description | Example |
|---------|-------------|---------|
| `label add` | Add labels | `br label add bd-abc123 backend urgent` |
| `label remove` | Remove label | `br label remove bd-abc123 urgent` |
| `label list` | List issue labels | `br label list bd-abc123` |
| `label list-all` | All labels in project | `br label list-all` |

### Comments

| Command | Description | Example |
|---------|-------------|---------|
| `comments add` | Add comment | `br comments add bd-abc123 "Found root cause"` |
| `comments list` | List comments | `br comments list bd-abc123` |

### Sync & System

| Command | Description | Example |
|---------|-------------|---------|
| `sync` | Sync DB ↔ JSONL | `br sync --flush-only` |
| `doctor` | Run diagnostics | `br doctor` |
| `stats` | Project statistics | `br stats` |
| `config` | Manage config | `br config --list` |
| `upgrade` | Self-update | `br upgrade` |
| `version` | Show version | `br version` |

### Global Flags

| Flag | Description |
|------|-------------|
| `--json` | JSON output (machine-readable) |
| `--quiet` / `-q` | Suppress output |
| `--verbose` / `-v` | Increase verbosity (-vv for debug) |
| `--no-color` | Disable colored output |
| `--db <path>` | Override database path |

---

## Configuration

br uses layered configuration:

1. **CLI flags** (highest priority)
2. **Environment variables**
3. **Project config**: `.beads/config.yaml`
4. **User config**: `~/.config/beads/config.yaml`
5. **Defaults** (lowest priority)

### Example Config

```yaml
# .beads/config.yaml

# Issue ID prefix (default: "bd")
id:
  prefix: "proj"

# Default values for new issues
defaults:
  priority: 2
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
```

### Config Commands

```bash
# Show all config
br config --list

# Get specific value
br config --get id.prefix

# Set value
br config --set defaults.priority=1

# Open in editor
br config --edit
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `BEADS_DB` | Override database path |
| `BEADS_JSONL` | Override JSONL path (requires `--allow-external-jsonl`) |
| `RUST_LOG` | Logging level (debug, info, warn, error) |

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                         CLI (br)                              │
│  Commands: create, list, ready, close, sync, etc.            │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│                      Storage Layer                            │
│  ┌─────────────────┐              ┌─────────────────────┐    │
│  │  SqliteStorage  │◄────────────►│  JSONL Export/Import │    │
│  │                 │   sync       │                     │    │
│  │  - WAL mode     │              │  - Atomic writes    │    │
│  │  - Dirty track  │              │  - Content hashing  │    │
│  │  - Blocked cache│              │  - Merge support    │    │
│  └────────┬────────┘              └──────────┬──────────┘    │
└───────────│──────────────────────────────────│───────────────┘
            │                                  │
            ▼                                  ▼
     .beads/beads.db                    .beads/issues.jsonl
     (Primary storage)                  (Git-friendly export)
```

### Data Flow

```
User Action                    br Command              Storage
───────────────────────────────────────────────────────────────
Create issue        ──►      br create        ──►    SQLite INSERT
                                              ──►    Mark dirty

Update issue        ──►      br update        ──►    SQLite UPDATE
                                              ──►    Mark dirty

Query issues        ──►      br list          ──►    SQLite SELECT

Export to git       ──►      br sync          ──►    Write JSONL
                             --flush-only     ──►    Clear dirty flags

Pull from git       ──►      git pull         ──►    JSONL updated
                    ──►      br sync          ──►    Merge to SQLite
                             --import-only
```

### Safety Model

br is designed to be **provably safe**:

| Guarantee | Implementation |
|-----------|----------------|
| Never executes git | No `Command::new("git")` calls in sync code |
| Only touches `.beads/` | Path validation before all writes |
| Atomic writes | Write to temp file, then rename |
| No data loss | Guards prevent overwriting non-empty JSONL with empty DB |

---

## Troubleshooting

### Error: "Database locked"

**Cause:** Another process has the database open.

```bash
# Check for other br processes
pgrep -f "br "

# Force close and retry
br sync --status  # Safe read-only check
```

### Error: "Issue not found"

**Cause:** Issue ID doesn't exist or was deleted.

```bash
# Check if issue exists
br list --json | jq '.[] | select(.id == "bd-abc123")'

# Check for similar IDs
br list | grep -i "abc"
```

### Error: "Prefix mismatch"

**Cause:** JSONL contains issues with different ID prefix.

```bash
# Check your prefix
br config --get id.prefix

# Import with validation skip (careful!)
br sync --import-only --skip-prefix-validation
```

### Error: "Stale database"

**Cause:** JSONL has issues that don't exist in database.

```bash
# Check sync status
br sync --status

# Force import (may lose local changes)
br sync --import-only --force
```

### Sync Issues After Git Merge

```bash
# 1. Check for JSONL merge conflicts
git status .beads/

# 2. If conflicts, resolve manually then:
br sync --import-only

# 3. If database seems stale:
br doctor
```

### Command Output is Garbled

```bash
# Disable colors
br list --no-color

# Or use JSON output
br list --json | jq
```

---

## Limitations

br intentionally does **not** support:

| Feature | Reason |
|---------|--------|
| **Automatic git commits** | Non-invasive philosophy |
| **Git hook installation** | User-controlled, add manually if desired |
| **Background daemon** | Simple CLI, no processes to manage |
| **Dolt backend** | SQLite + JSONL only |
| **Linear/Jira sync** | Focused scope |
| **Web UI** | CLI-first (see beads_viewer for TUI) |
| **Multi-repo sync** | Single repo per workspace |
| **Real-time collaboration** | Git-based async collaboration |

---

## FAQ

### Q: How do I integrate with beads_viewer (bv)?

br works seamlessly with [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer):

```bash
# Use bv for interactive TUI
bv

# Use br for CLI/scripting
br ready --json | jq
```

### Q: Can I use br with AI coding agents?

Yes! br is designed for AI agent integration:

```bash
# Agents can use --json for structured output
br list --json
br ready --json
br show bd-abc123 --json

# Create issues programmatically
br create "Title" --json  # Returns created issue as JSON
```

See [AGENTS.md](AGENTS.md) for the complete agent integration guide.

### Q: How do I migrate from the original beads?

br uses the same JSONL format as classic beads:

```bash
# Copy your existing issues.jsonl
cp /path/to/beads/.beads/issues.jsonl .beads/

# Import into br
br sync --import-only
```

### Q: Why Rust instead of Go?

- **Smaller binary:** ~5-8 MB vs ~30+ MB
- **Memory safety:** No runtime garbage collection
- **Stability:** Fewer moving parts = fewer things to break
- **Personal preference:** The author's flywheel tooling is Rust-based

### Q: How do dependencies work?

```bash
# Issue A depends on Issue B (A is blocked until B is closed)
br dep add bd-A bd-B

# Now bd-A won't appear in `br ready` until bd-B is closed
br ready  # Only shows bd-B

# Close the blocker
br close bd-B

# Now bd-A is ready
br ready  # Shows bd-A
```

### Q: How do I handle merge conflicts in JSONL?

JSONL is line-based, so conflicts are usually easy to resolve:

```bash
# After git merge with conflicts
git status .beads/issues.jsonl

# Edit to resolve (each line is one issue)
vim .beads/issues.jsonl

# Mark resolved and import
git add .beads/issues.jsonl
br sync --import-only
```

### Q: Can I customize the issue ID prefix?

Yes:

```bash
br config --set id.prefix=myproj
# New issues: myproj-abc123
```

### Q: Where is data stored?

```
.beads/
├── beads.db        # SQLite database (primary storage)
├── issues.jsonl    # JSONL export (for git)
├── config.yaml     # Project configuration
└── metadata.json   # Workspace metadata
```

---

## AI Agent Integration

br is designed for AI coding agents. See [AGENTS.md](AGENTS.md) for:

- JSON output schemas
- Workflow patterns
- Integration with MCP Agent Mail
- Robot mode flags
- Best practices

Quick example:

```bash
# Agent workflow
bd ready --json | jq '.[0]'           # Get top priority
bd update bd-abc --status in_progress # Claim work
# ... do work ...
bd close bd-abc --reason "Completed"  # Done
bd sync --flush-only                  # Export for git
```

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License - See [LICENSE](LICENSE) for details.

---

<div align="center">
  <sub>Built with Rust. Powered by SQLite. Synced with Git.</sub>
</div>
