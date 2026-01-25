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

[Quick Start](#quick-start) | [Commands](#commands) | [Configuration](#configuration) | [VCS Integration](#vcs-integration) | [FAQ](#faq)

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

