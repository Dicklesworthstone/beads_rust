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

## Output Modes

br supports multiple output modes for different use cases:

| Mode | When Active | Description |
|------|-------------|-------------|
| **Rich** | TTY with colors | Colored panels, tables, styled text |
| **Plain** | `NO_COLOR` env or `--no-color` | Text output without ANSI codes |
| **JSON** | `--json` or `--robot` | Machine-readable structured output |
| **Quiet** | `--quiet` or `-q` | Minimal output |

### Mode Detection

The output mode is automatically detected:

1. `--json` or `--robot` flags → **JSON mode**
2. `--quiet` flag → **Quiet mode**
3. `NO_COLOR` env var or `--no-color` → **Plain mode**
4. Non-TTY stdout (piped output) → **Plain mode**
5. Otherwise → **Rich mode** (default for interactive terminals)

### For Coding Agents

**CRITICAL:** Always use `--json` or `--robot` flags when parsing br output programmatically.

```bash
# CORRECT - stable, parseable output
br list --json | jq '.[0]'
br ready --robot

# WRONG - output format may vary based on terminal state
br list | head -1
```

JSON mode guarantees:
- Stable schema (changes are versioned and documented)
- No ANSI escape codes
- Clean stdout (diagnostics go to stderr)
- Exit codes for success/failure

### Rich Output Features

In Rich mode, br provides enhanced terminal output:

- **Tables** - Formatted with borders and alignment
- **Panels** - Boxed content with titles
- **Status icons** - ✓ success, ✗ error, ⚠ warning
- **Colors** - Priority-based coloring, status highlighting
- **Progress** - Spinners for long operations

These features are **automatically disabled** when:
- Output is piped to another command
- `NO_COLOR` environment variable is set
- `--no-color` flag is used
- `--json` mode is active

### Output Style Guidelines

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

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads Rust (br) — Dependency-Aware Issue Tracking

br (beads_rust) provides a lightweight, dependency-aware issue database and CLI for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `proj-abc12`) as Mail `thread_id` and prefix subjects with `[proj-abc12]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="proj-abc12")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="proj-abc12", subject="[proj-abc12] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close proj-abc12 --reason "Completed"
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[proj-abc12] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `<issue-id>` (e.g., `proj-abc12`) |
| Mail subject | `[<issue-id>] ...` |
| File reservation `reason` | `<issue-id>` |
| Commit messages | Include `<issue-id>` for traceability |

### Copyable AGENTS.md Blurb

Add this section to your project's AGENTS.md to enable br-aware agents:

```markdown
## Beads Rust (br) — Dependency-Aware Issue Tracking

br provides a lightweight, dependency-aware issue database and CLI for selecting "ready work," setting priorities, and tracking status.

### Essential Commands

\`\`\`bash
br ready              # Show issues ready to work (no blockers)
br list --status open # All open issues
br show <id>          # Full issue details with dependencies
br create --title "Fix bug" --type bug --priority 2 --description "Details here"
br update <id> --status in_progress
br close <id> --reason "Completed"
br sync               # Export to JSONL for git sync
\`\`\`

### Workflow Pattern

1. **Start**: Run `br ready --json` to find actionable work
2. **Claim**: Use `br update <id> --status in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id> --reason "Done"`
5. **Sync**: Always run `br sync` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog
- **Types**: task, bug, feature, epic, question, docs
- **JSON output**: Always use `--json` or `--robot` when parsing programmatically
```

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use ONLY `--robot-*` flags. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** It returns:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command
```

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |
| `--robot-label-health` | Per-label health: `health_level`, `velocity_score`, `staleness`, `blocked_count` |

### jq Quick Reference

```bash
bv --robot-triage | jq '.quick_ref'                        # At-a-glance summary
bv --robot-triage | jq '.recommendations[0]'               # Top recommendation
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs <changed-files>` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs file.rs file2.rs                    # Specific files (< 1s) — USE THIS
ubs $(git diff --name-only --cached)    # Staged files — before commit
ubs --only=rust,toml src/               # Language filter (3-5x faster)
ubs --ci --fail-on-warning .            # CI mode — before PR
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
⚠️  Category (N errors)
    file.rs:42:5 – Issue description
    💡 Suggested fix
Exit code: 1
```

Parse: `file:line:col` → location | 💡 → how to fix | Exit 0/1 → pass/fail

### Fix Workflow

1. Read finding → category + fix suggestion
2. Navigate `file:line:col` → view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-run `ubs <file>` → exit 0
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** → `ast-grep`
- Need raw speed or **hunting text** → `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How is pattern matching implemented?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is the quick reject filter?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `Regex::new`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/path/to/project",
  query: "How does the safe pattern whitelist work?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name → use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" → wastes time with manual reads
- **Don't** use `ripgrep` for codemods → risks collateral edits

---

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations (Claude Code, Codex, Cursor, Gemini, ChatGPT, etc.) so we can reuse solved problems.

**Rules:** Never run bare `cass` (TUI). Always use `--robot` or `--json`.

### Examples

```bash
cass health
cass search "authentication error" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json
cass capabilities --json
cass robot-docs guide
```

### Tips

- Use `--fields minimal` for lean output
- Filter by agent with `--agent`
- Use `--days N` to limit to recent history

stdout is data-only, stderr is diagnostics; exit code 0 means success.

Treat cass as a way to avoid re-solving problems other agents already handled.

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

This project uses [beads_rust (br)](https://github.com/Dicklesworthstone/beads_rust) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title "..." --type task --priority 2
br update <id> --status in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync               # Export to JSONL for git sync
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

<!-- end-bv-agent-instructions -->

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   br sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

---

Note for Codex/GPT-5.2:

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/cli/commands/upgrade.rs, src/storage/sqlite.rs, tests/conformance.rs, tests/storage_deps.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
2. Triage beads_rust-orko (clippy/cargo warnings) and beads_rust-ydqr (rustfmt failures).
3. If you want a full suite run later, fix conformance/clippy blockers and re‑run cargo test --all.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into think YOU made the changes and simply don't recall it for some reason.
