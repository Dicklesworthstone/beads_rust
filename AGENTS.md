# AGENTS.md — beads_rust (br)

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml only
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]` via crate lints)

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI parsing with derive macros |
| `rusqlite` | SQLite storage (bundled, modern_sqlite features) |
| `serde` + `serde_json` | Issue serialization and JSONL export |
| `chrono` | Timestamp parsing and RFC3339 formatting |
| `rayon` | Parallel processing |
| `tracing` | Structured logging |
| `anyhow` + `thiserror` | Error handling |
| `sha2` | Content hashing for dedup |

### Release Profile

The release build optimizes for binary size:

```toml
[profile.release]
opt-level = "z"     # Optimize for size (lean binary for distribution)
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
panic = "abort"     # Smaller binary, no unwinding overhead
strip = true        # Remove debug symbols
```

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `mainV2.rs`
- `main_improved.rs`
- `main_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Project Semantics (beads_rust / br)

This tool is a Rust port of the "classic" beads issue tracker (SQLite + JSONL hybrid). Keep these invariants intact:

- **Isomorphic to Go beads:** The Rust `br` command should produce identical output to the Go `bd` command for equivalent inputs. Test harnesses validate this.
- **SQLite + JSONL hybrid:** Primary storage is SQLite; JSONL export is for git-based sync and human readability. No Dolt backend.
- **Schema compatibility:** Database schema must match Go beads schema for potential cross-tool usage.
- **CLI compatibility:** Command names, flags, and output formats should match Go beads where sensible.
- **ID format:** Use hash-based short IDs (e.g., `bd-abc123`), not auto-increment integers.
- **Content hashing:** Issues have deterministic content hashes for deduplication.

### Key Design: Non-Invasive

**`br` is LESS invasive than `bd`:**

- **No automatic git hooks** — Users add hooks manually if desired
- **No automatic git operations** — No auto-commit, no auto-push
- **No daemon/RPC** — Simple CLI only, no background processes
- **Explicit over implicit** — Every git operation requires explicit user command

### What We're NOT Porting

- **Dolt backend:** The entire `internal/storage/dolt/` package is excluded. SQLite only.
- **RPC daemon:** Non-invasive design means no background processes.
- **Git hooks:** No automatic hook installation. Users add manually.
- **Linear/Jira integration:** External service integrations deferred.
- **Claude plugin:** MCP plugin is separate; port core CLI first.
- **Gastown features:** All agent/molecule/gate/rig/convoy/HOP features excluded (see PLAN doc for full list).

---

## Output Style

- **Text output** is user-facing and may include color. Avoid verbose debug spew unless `--verbose` is set.
- **JSON output** must be stable and machine-parseable. Do not change JSON shapes without explicit intent and tests.
- **Robot mode:** Support `--json` and `--robot` flags for machine-readable output (clean JSON to stdout, diagnostics to stderr).

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings
cargo check --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Testing

### Unit Tests

```bash
cargo test
cargo test -- --nocapture
```

### Focused Tests

```bash
cargo test storage
cargo test cli
cargo test export
```

### Conformance Tests

Once basic functionality works, we'll create conformance tests that:
1. Run equivalent commands on both `bd` (Go) and `br` (Rust)
2. Compare outputs (JSON mode) for identical results
3. Validate database schema compatibility

---

## Sync Safety Maintenance

When modifying sync-related code (`src/sync/`, `src/cli/commands/sync.rs`), you MUST follow the maintenance checklist:

**See: [`docs/SYNC_MAINTENANCE_CHECKLIST.md`](docs/SYNC_MAINTENANCE_CHECKLIST.md)**

Quick summary:
1. **No git operations** — Static check: `grep -rn 'Command::new.*git' src/sync/`
2. **Path allowlist** — Verify only `.beads/` files are touched
3. **Run safety tests** — `cargo test e2e_sync --release`
4. **Review logs** — Check for unexpected safety events
5. **Update docs** — If behavior changed

Related documentation:
- [SYNC_SAFETY.md](docs/SYNC_SAFETY.md) — User-facing safety model
- [E2E_SYNC_TESTS.md](docs/E2E_SYNC_TESTS.md) — Test execution guide
- [.beads/SYNC_SAFETY_INVARIANTS.md](.beads/SYNC_SAFETY_INVARIANTS.md) — Technical invariants

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and best practices before coding. Prefer primary docs.

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

### Rule of Thumb

- Need correctness or **applying changes** → `ast-grep`
- Need raw speed or **hunting text** → `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

---

## Session Completion

Before ending a work session:

1. Summarize changes clearly
2. Note any remaining risks or follow-ups
3. Provide the exact commands to run for tests/linters (if not run)

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Sync beads** - Run `bd sync --flush-only` to export issues to JSONL
5. **Commit changes** - All changes committed
6. **Hand off** - Provide context for next session

**Note:** No git remote is configured for this project. Work is local-only.

---

## Issue Tracking with bd (beads)

All issue tracking goes through **bd**. No other TODO systems.

Key invariants:

- `.beads/` is authoritative state and **must always be committed** with code changes.
- Do not edit `.beads/*.jsonl` directly; only via `bd`.

### Basics

Check ready work:

```bash
bd ready --json
```

Create issues:

```bash
bd create "Issue title" -t bug|feature|task -p 0-4 --json
bd create "Issue title" -p 1 --deps discovered-from:bd-123 --json
```

Update:

```bash
bd update bd-42 --status in_progress --json
bd update bd-42 --priority 1 --json
```

Complete:

```bash
bd close bd-42 --reason "Completed" --json
```

Types:

- `bug`, `feature`, `task`, `epic`, `chore`

Priorities:

- `0` critical (security, data loss, broken builds)
- `1` high
- `2` medium (default)
- `3` low
- `4` backlog

Agent workflow:

1. `bd ready` to find unblocked work.
2. Claim: `bd update <id> --status in_progress`.
3. Implement + test.
4. If you discover new work, create a new bead with `discovered-from:<parent-id>`.
5. Close when done.
6. Commit `.beads/` in the same commit as code changes.

Auto-sync:

- bd exports to `.beads/issues.jsonl` after changes (debounced).
- It imports from JSONL when newer (e.g. after `git pull`).

Never:

- Use markdown TODO lists.
- Use other trackers.
- Duplicate tracking.

---

## Using bv as an AI Sidecar

`bv` is a terminal UI + analysis layer for `.beads/beads.jsonl`. It precomputes graph metrics so you don't have to.

Useful robot commands:

- `bv --robot-help` – overview
- `bv --robot-insights` – graph metrics (PageRank, betweenness, HITS, critical path, cycles)
- `bv --robot-plan` – parallelizable execution plan with unblocks info
- `bv --robot-priority` – priority suggestions with reasoning
- `bv --robot-recipes` – list recipes; apply via `bv --recipe <name>`
- `bv --robot-diff --diff-since <commit|date>` – JSON diff of issue changes

Use `bv` instead of rolling your own dependency graph logic.

---

## Reference Projects

This project follows patterns established in two sibling Rust CLI projects:

### xf (X Archive Finder)
- Location: `/data/projects/xf`
- Full-text search with Tantivy
- SQLite storage with WAL mode and optimized pragmas
- Clap derive-based CLI

### cass (Coding Agent Session Search)
- Location: `/data/projects/coding_agent_session_search`
- Streaming indexing with producer-consumer channels
- Prefix caching with Bloom filters
- Custom error types with CliError struct

When implementing new features, consult these projects for idiomatic Rust patterns.

---

## Legacy Beads Reference

The original Go implementation is in `./legacy_beads/` for reference (gitignored). Key directories:

- `internal/storage/sqlite/` — SQLite backend (PORT THIS)
- `internal/types/` — Data models (PORT THIS)
- `cmd/bd/` — CLI commands (PORT THIS)
- `internal/storage/dolt/` — Dolt backend (DO NOT PORT)

---

<!-- bv-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
bd ready              # Show issues ready to work (no blockers)
bd list --status=open # All open issues
bd show <id>          # Full issue details with dependencies
bd create --title="..." --type=task --priority=2
bd update <id> --status=in_progress
bd close <id> --reason="Completed"
bd close <id1> <id2>  # Close multiple issues at once
bd sync --flush-only  # Export to JSONL only (no git push)
```

### Workflow Pattern

1. **Start**: Run `bd ready` to find actionable work
2. **Claim**: Use `bd update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `bd close <id>`
5. **Sync**: Always run `bd sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `bd ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `bd dep add <issue> <depends-on>` to add dependencies
