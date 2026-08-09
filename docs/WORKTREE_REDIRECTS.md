# Shared br Workspaces Across Git Worktrees

Git worktrees should normally share one tracker authority: the primary
worktree's complete `.beads` directory. A secondary worktree contains only a
local `.beads/redirect` file. Database, JSONL, configuration, policy, metadata,
locks, and issue state remain owned by the canonical primary workspace.

`br where` is the inspection surface for the active workspace and reports the
redirect origin when one is in use. Redirect setup never runs Git, initializes
or repairs the target, migrates its schema, imports or exports JSONL, or mutates
the target.

## Create a Redirected Worktree Workspace

From a new secondary worktree, use automatic discovery for a standard linked
worktree layout:

```bash
br init --redirect
```

`br` reads the worktree's `.git` file, `commondir`, and primary-worktree
backlink directly. It does not launch Git. If the layout is bare, separated,
malformed, or ambiguous, provide the exact initialized tracker directory:

```bash
br init --redirect /absolute/path/to/primary/.beads
```

The target must already exist, be initialized, and be usable by the running
`br` version. Repeating the same request is a byte-preserving no-op. A
conflicting redirect is preserved and rejected. Running the automatic command
in the primary worktree is also a successful no-op.

Use global `--json` for the stable `br.redirect.v1` receipt. It reports the
local source, requested and final target, explicit or automatic selection,
changed/no-op disposition, primary-owner state, acknowledgement state, and all
dormant local artifacts.

## Adopt an Already Initialized Worktree

Existing local tracker state is never deleted, moved, overwritten, or silently
shadowed. Inspect it, then explicitly acknowledge that it will become dormant:

```bash
br redirect set --allow-existing
br redirect set /absolute/path/to/primary/.beads --allow-existing
```

Without `--allow-existing`, material local artifacts cause a nonzero refusal.
With acknowledgement, every local file, symlink, permission, timestamp, DB
family member, JSONL file, configuration file, and policy file remains in
place. The receipt inventories the dormant top-level artifacts. A repeated
same-target invocation is a no-op and does not require another acknowledgement.

There is intentionally no redirect-removal command. Recovery or authority
splitting requires an operator-reviewed migration of the dormant and canonical
state, not automatic unlinking.

## Codex, Claude, and Worktrunk Lifecycle Adapters

The tracked `.codex/hooks.json` is the default repository-owned path for
Codex-managed worktrees. Its `SessionStart` hook runs `br init --redirect`
directly from the session working directory on `startup` and `resume`. It does
not run on `clear` or `compact`, add repository instructions to the session, or
perform any other startup work. Successful creation, primary-owner detection,
and an existing same-target redirect are quiet, successful no-ops where
appropriate.

Codex hooks are enabled by default, but that does not bypass trust. The project
`.codex/` layer must be trusted, and Codex separately requires review of the
exact hook definition for a non-managed command. A new or changed definition is
skipped until it is trusted. Use `/hooks` to inspect the source and review the
current definition. `--dangerously-bypass-hook-trust` exists for one-off
automation that independently vets hook sources; it bypasses persisted
definition trust for that invocation and should not be treated as repository
approval. See the [official Codex hooks
documentation](https://developers.openai.com/codex/hooks) for the current trust
and lifecycle contract.

The Codex adapter is revision-local: historical revisions without the tracked
`.codex/hooks.json` do not run it, and setting `[features].hooks = false` also
bypasses it. A failed command—including missing `br`, unusable or ambiguous
automatic discovery, a conflicting redirect, or initialized local tracker
state—returns a concise Codex warning while the already-started session
continues. Routing and local tracker artifacts remain unchanged. Recover in the
session worktree with `br init --redirect` or the exact-path form. If local
tracker state is already initialized, inspect it and explicitly acknowledge
adoption with `br redirect set --allow-existing`; repository automation never
supplies that flag.

The repository's Claude `PostToolUse` adapter for `EnterWorktree` and its
Worktrunk `pre-start` hook call `br init --redirect` directly from the created
worktree. Success, primary-owner, and already-configured results are quiet.
Failure prints an actionable warning and exits successfully because the
worktree already exists.

Worktrunk treats repository hooks as untrusted until the operator approves the
exact command. Review `.config/wt.toml`, then approve it interactively:

```bash
wt config approvals add
```

Agents must not approve project hooks or use `--yes` to bypass this trust
decision. `wt switch --no-hooks ...` bypasses the adapter. Recover afterward
from the created worktree with `br init --redirect` or the exact-path form.

## Raw Git Worktree Automation

Raw `git worktree add` is covered only after an explicit clone-local operator
action:

```bash
scripts/activate-worktree-redirect-hook.sh
```

The activation utility installs the reviewed dispatcher only at the common Git
directory's `hooks/post-checkout`. It leaves `core.hooksPath` unchanged. It is
an idempotent success when the identical executable dispatcher is already
installed, and otherwise refuses to replace a hook, symlink, or configured
hooks path. Preserve and manually chain existing hook infrastructure after
reviewing both behaviors.

The dispatcher calls `br init --redirect` directly and exits successfully even
when setup fails, while printing a prominent recovery warning. Because it lives
in the common hooks directory, it covers linked worktrees created from
historical revisions that do not contain current repository lifecycle files.
It requires both Git's null previous-HEAD value and branch-checkout flag, so an
ordinary `git checkout` or `git switch` in an existing worktree never adopts or
changes that worktree's tracker routing.
Clones without activation and Git invocations with hooks disabled remain
unchanged.

`git worktree add --no-checkout` bypasses `post-checkout`. Complete setup
manually in the new worktree:

```bash
br init --redirect /absolute/path/to/primary/.beads
```

Codex, Claude, Worktrunk, and native Git may all invoke setup for the same
worktree. The Codex adapter is the default for Codex sessions; native Git is an
optional, earlier, client-independent layer. Matching repeated or concurrent
requests converge on the same redirect; none creates a second database or
tracker authority.
