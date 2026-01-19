# br - Beads Rust

A fast, non-invasive issue tracker for git repositories. Rust port of [beads](https://github.com/Dicklesworthstone/beads).

---

## Quick Install

```bash
# Build from source (requires Rust nightly)
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

Or clone and build:

```bash
git clone https://github.com/Dicklesworthstone/beads_rust.git
cd beads_rust
cargo build --release
./target/release/br --help
```

### Self-Update (enabled by default)

The `br upgrade` command is enabled by default. To disable self-update support, build without default features:

```bash
# Disable self-update
cargo build --release --no-default-features

# Or install without self-update
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git --no-default-features
```

---

## TL;DR

br is a local-first issue tracker that stores issues in SQLite with JSONL export for git-based collaboration. It's designed to be **non-invasive**: no daemons, no git hooks, no auto-commits.

```bash
br init                              # Initialize in current repo
br create "Fix login bug" -p 1       # Create high-priority bug
br list                              # Show all issues
br ready                             # Show actionable work
br close bd-abc123                   # Close an issue
br sync --flush-only                 # Export to JSONL for git
```

---

## Why This Project Exists

I (Jeffrey Emanuel) absolutely love [Steve Yegge's Beads project](https://github.com/steveyegge/beads). Discovering it was one of those serendipitous moments that unlocked amazing synergies for my Flywheel Tooling system, particularly when combined with my [MCP Agent Mail](https://github.com/Dicklesworthstone/mcp-agent-mail) and [beads_viewer (bv)](https://github.com/Dicklesworthstone/beads_viewer) projects. I'm deeply grateful for finding it and for Steve's work.

However, at this point, my flywheel is incredibly dependent on beads operating in a certain way. The beads project has grown significantly over the past month in terms of lines of code, features, and functionality. In some cases, this growth has gone beyond what I need for the flywheel system, and some new features have made it more invasive in the system/projects/repos and somewhat unpredictable.

The recent changes to align beads with [GasTown](https://github.com/steveyegge/gastown) have introduced additional complexity that I wasn't using. The decision to move away from the hybrid SQLite + JSONL-git approach—an architectural decision that I loved and independently mirrored in the design of MCP Agent Mail—was the final push that led me to create this Rust port of "pre-GasTown Classic Beads."

The command is `br` to distinguish it from the original beads' `bd` command.

**This decision is not a statement about beads or the changes being bad.** It's simply that I need a stable, simple version of beads for my own tooling that isn't undergoing so much flux. Rapid changes can be disruptive—for example, a recent sync issue accidentally deleted all code files in a project. That's exactly the kind of thing I'd rather avoid with a simpler, smaller codebase.

| Project | Lines of Code |
|---------|---------------|
| beads_rust (br) | ~20,000 lines of Rust |
| beads (bd) | ~276,000 lines of Go |

beads_rust intentionally stays small and focused on the classic hybrid SQLite + JSONL architecture that made beads so elegant for git-based collaboration.

---

## Features

| Feature | Status | Description |
|---------|--------|-------------|
| Issue CRUD | :white_check_mark: | Create, read, update, delete issues |
| Dependencies | :white_check_mark: | Block/unblock relationships |
| Labels | :white_check_mark: | Categorize with custom labels |
| Search | :white_check_mark: | Full-text search across issues |
| JSONL Sync | :white_check_mark: | Git-friendly export/import |
| Comments | :white_check_mark: | Issue discussion threads |
| Stats | :white_check_mark: | Project statistics and health |
| JSON Output | :white_check_mark: | Machine-readable output (`--json`) |
| Config System | :white_check_mark: | YAML-based configuration |
| History Backups | :white_check_mark: | Local history with restore |

---

## AI Agent Integration

br works seamlessly with AI coding agents. Use the `--json` flag for machine-parseable output:

```bash
# Structured JSON output for all commands
br list --json
br ready --json
br show bd-123 --json

# Create and update with JSON response
br create "Fix bug" --type task --json
br close bd-123 --json
```

For advanced agent features (robot mode, graph analysis, TUI), see [beads_viewer (bv)](https://github.com/Dicklesworthstone/beads_viewer).

See [AGENTS.md](AGENTS.md) for the complete agent integration guide.

---

## Quick Example

```bash
# Initialize br in your project
cd my-project
br init

# Create your first issue
br create --title "Implement user authentication" --type feature --priority 1

# Add a dependency
br create --title "Set up database schema" --type task
br dep add bd-xyz bd-abc  # xyz depends on abc (blocked by)

# See what's ready to work on
br ready

# Claim work
br update bd-abc --status in_progress

# Complete and sync
br close bd-abc --reason "Schema implemented"
br sync --flush-only
git add .beads/ && git commit -m "Update issues"
```

---

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│   CLI (br)  │────>│ SQLite Store │────>│ JSONL Sync  │
└─────────────┘     └──────────────┘     └─────────────┘
                           │                    │
                           v                    v
                    .beads/beads.db      .beads/issues.jsonl
```

- **SQLite**: Primary storage, WAL mode, concurrent access
- **JSONL**: Git-friendly export for collaboration
- **No daemon**: Simple CLI, no background processes
- **Non-invasive**: Never executes git commands or modifies source files

---

## Safety Model

br is designed to be **provably safe**. The sync command:

- **Never executes git commands** - No commits, no pushes, no staging
- **Never modifies files outside `.beads/`** - Your source code is untouched
- **Uses atomic writes** - Partial failures don't corrupt data
- **Has export guards** - Prevents accidental data loss

See [docs/SYNC_SAFETY.md](docs/SYNC_SAFETY.md) for the complete safety model.

---

## Commands

| Command | Description |
|---------|-------------|
| `br init` | Initialize a beads workspace |
| `br create` | Create a new issue |
| `br q` | Quick capture (create, print ID only) |
| `br list` | List issues |
| `br show` | Show issue details |
| `br update` | Update an issue |
| `br close` | Close an issue |
| `br reopen` | Reopen a closed issue |
| `br delete` | Delete an issue (creates tombstone) |
| `br ready` | List ready issues (unblocked, not deferred) |
| `br blocked` | List blocked issues |
| `br search` | Search issues |
| `br dep` | Manage dependencies |
| `br label` | Manage labels |
| `br comments` | Manage comments |
| `br stats` | Show project statistics |
| `br count` | Count issues with grouping |
| `br stale` | List stale issues |
| `br config` | Configuration management |
| `br sync` | Sync database with JSONL |
| `br doctor` | Run diagnostics |
| `br version` | Show version information |
| `br upgrade` | Self-update to latest release (requires `self_update` feature) |

Use `br <command> --help` for detailed command options.

---

## Configuration

br uses a layered configuration system:

1. **Project config**: `.beads/config.yaml` (per-repository)
2. **User config**: `~/.config/beads/config.yaml` (global defaults)

```bash
# Show current configuration
br config --list

# Get a specific value
br config --get id.prefix

# Set a value
br config --set id.prefix=proj

# Open in editor
br config --edit
```

---

## JSONL Sync Workflow

br separates issue tracking from git operations for safety:

```bash
# After making changes
br sync --flush-only          # Export DB to JSONL
git add .beads/               # Stage manually
git commit -m "Update issues" # Commit manually

# After pulling changes
git pull
br sync --import-only         # Import JSONL to DB

# Check sync status
br sync --status
```

---

## Contributing

See [AGENTS.md](AGENTS.md) for coding guidelines. Key points:

- Rust 2024 edition (nightly required)
- No unsafe code
- Run `cargo test` before submitting
- Follow existing code style

---

## License

MIT License
