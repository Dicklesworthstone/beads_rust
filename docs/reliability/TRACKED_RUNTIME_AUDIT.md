# Audit Git-tracked tracker runtime files

`scripts/br_runtime_audit.py` is an explicitly invoked, read-only diagnostic for
beads_rust-9uavz. It can audit one worktree or a fleet before an operator updates
Git. It does not run automatically, change `br doctor` exit codes, open a database,
or change the index, worktree, or refs. Python 3.10+ and Git are required; no
Python packages need to be installed.

```sh
python3 scripts/br_runtime_audit.py /path/to/repo --json
python3 scripts/br_runtime_audit.py /path/to/repo-a /path/to/repo-b --json
python3 scripts/br_runtime_audit.py /path/to/repo --incoming-ref origin/main --json
python3 scripts/br_runtime_audit.py /path/to/repo \
  --beads-dir tracker/.beads --database cache/tracker --json
```

Pass worktree roots, not subdirectories. `--beads-dir` is repeatable and replaces
the default `.beads` scope. `--database` adds exact repository-relative database
names and their sidecars, including extensionless names. The auditor does not
infer custom paths from metadata or read files behind runtime symlinks.

## What the result establishes

The auditor independently reads HEAD, index stages, and optionally an
already-local incoming commit. Ignore rules do not suppress tracked-file
findings. It identifies staged untracking, unresolved index stages, and incoming
removals, replacements, or additions of recognized runtime paths. Recovery
namespace contents are included even when their leaf names look like shared JSON.
Git replacement refs are disabled so they cannot mask actual commit contents.

Legacy recovery files beside a database are included as well: `.bad` and
`.corrupt` with an end-of-name or `_`, `-`, or `.` boundary, and `.stale_` or
`.rebuild_` suffixes. These names must belong to a `.db`, `.sqlite`, or `.sqlite3`
database or known sidecar inside the selected metadata directory, or to an exact
explicit `--database` family. Newly recognized legacy files use the existing
`recovery` kind; existing sidecar categories keep their meaning. Ordinary names
such as `beads.db.badger.md`, neighboring trackers, and files beneath a lookalike
directory do not become recovery findings.

Exit 0 means no recognized tracked runtime paths in the requested scope. Exit 1
means findings exist. Exit 2 means some evidence was unavailable or incomplete;
fleet output retains successful observations from other repositories. JSON uses
`br.git-runtime-audit.v1`, includes path bytes in hex, and escapes non-UTF-8 and
terminal control characters. `observation_atomic` is always false: repeated
checks detect observed ref/index changes, not every possible concurrent race.

Probes disable optional locks, hooks, filters, fsmonitor, prompts, inherited Git
redirections and network protocols. They have one deadline per repository and
per-stream output caps. Ancestor metadata is included to detect requested scopes
hidden beneath tracked symlinks or submodules; a wider ancestor listing can
exhaust the same output cap and must then report unavailable, not clean. Capture
files are outside the worktree. The direct process is killed and reaped on a
probe failure; cleanup may exceed the deadline. The selected Git executable is
trusted, and this is not a sandbox for arbitrary descendants.

## Do not confuse detection with safe remediation

**Do not naively run `git rm --cached` and tell other clones to pull.** A commit
that removes a tracked runtime path can delete another clone's copy on update.
Adding `.gitignore` alone does not protect already-tracked files.

The auditor neither fetches nor checks out the incoming ref. It does not back up
live database families, stop writers, perform untracking, authorize certificate
quarantine, or establish cross-clone safety. Coordinated quiescence and verified
preservation of each clone's complete local family outside Git's worktree remain
required for remediation. Untracked runtime files and remote-only commits are
outside this diagnostic's evidence. A clean audit is not a healthy-database or
safe-to-delete receipt.

## Regression tests

```sh
python3 -m unittest discover -s tests -p 'test_br_runtime_audit.py' -v
```

Tests use disposable local Git repositories and no engine dependency. They check
HEAD/index differences, incoming changes, linked worktrees, SHA-256 repositories,
merge stages, hostile path names, bounds, and exact worktree/index byte and inode
preservation. Legacy evidence is tested as the sole tracked finding, after staged
untracking, and before incoming deletion or replacement; explicit extensionless
families and ordinary-name negatives constrain its scope. The two-clone control
first verifies the read-only warning, then
performs a synthetic untracking transition outside the auditor to demonstrate
that Git really removes the peer file. Fixtures are retained under the system
temporary directory for inspection. These tests do not replace Rust or release
qualification.
