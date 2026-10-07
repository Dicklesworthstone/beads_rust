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

## Offline preservation and independent recovery

`scripts/br_runtime_snapshot.py` implements the per-clone preservation and
recovery boundary. It is a separate, explicitly invoked POSIX/Python 3.10+ tool;
the auditor and native `br vcs-status --runtime-files` remain observational.
No Python packages are required. The snapshot tool never invokes Git or SQLite.

**Stop every tracker user before capture and keep them stopped throughout the
operator-controlled Git transition and live-installation review.** The required
`--writers-stopped` flag records the operator's assertion. It does not acquire
engine/opener locks, stop processes, or make this an online backup. Repeated
identity, namespace and byte checks detect observed changes; they cannot prove
quiescence or prevent a transaction from straddling separate file copies.

Choose the complete directory containing the database, its journals/WAL,
certificates, namespace/migration state, and retained recovery evidence. The
tool copies every regular file and empty directory, including untracked files
and unknown future sidecars. It does not infer custom database paths from
metadata, CLI overrides, or environment variables. A database stored outside
`.beads` needs its own complete directory captured during the same stopped
interval. Do not treat a metadata-only snapshot as coverage of an external DB.

```sh
# Use a new bundle path outside all Git worktrees; its parent must exist.
python3 scripts/br_runtime_snapshot.py capture \
  /path/to/clone-b/.beads /private/backups/clone-b-before --writers-stopped

# Retain the printed manifest_sha256 separately from the bundle.
python3 scripts/br_runtime_snapshot.py verify \
  /private/backups/clone-b-before --manifest-sha256 SAVED_SHA256

# Recover ALL saved paths into a different new directory, never over live files.
python3 scripts/br_runtime_snapshot.py materialize \
  /private/backups/clone-b-before /private/backups/clone-b-recovered \
  --manifest-sha256 SAVED_SHA256
```

A successful materialization produces `clone-b-recovered/data/` plus a
`recovery.json` receipt after full byte readback and synchronization. Files are
0600 and directories 0700. Original modes are recorded but not reinstated;
ownership, ACLs, extended attributes and timestamps are not restored. Inspect
permissions and paths before any separately reviewed live installation. The
snapshot never merges newly pulled shared JSONL/configuration into the saved
local generation, swaps directories, changes live configuration, or selects
individual certificates for replacement. Keep each clone's own family together.

Capture/verification/materialization return 0 only on success, or 2 for refusal
or incomplete work, with JSON output. Existing destinations are refused. Failed
output is retained for inspection and must not be treated as ready; retry with
a new destination. Capture checks source bytes again and rechecks the entire
namespace; materialization validates the complete bundle before creating output,
hashes each copied object again, and verifies the complete recovered directory.
The separately saved manifest hash is the trust anchor. A digest recomputed from
an untrusted or edited manifest is not equivalent evidence.

Source links, multiply linked files, special files, different-device subtrees,
`.git` metadata, ambiguous roots, overlapping outputs and symlinked ancestors
are refused. Output checks conventional `.git` directory/gitfile worktrees;
this is not a sandbox for privileged mount changes or concurrent same-user
filesystem manipulation. The default byte limit is 4 GiB, with 100,000 entries,
64 path components, a 32 MiB manifest cap and a 300-second cooperative deadline.
`--max-bytes` and `--timeout` can raise the byte/time budgets. Blocking filesystem
calls and `fsync` cannot be preempted by the cooperative deadline.

This supplies recoverable per-clone copies, not an automatic cross-clone
untracking/checkout protocol. Every affected clone still requires coordination,
its own verified snapshot, and engine-specific validation of the separately
recovered generation before live installation. No snapshot or materialization
receipt certifies database health or authorizes certificate quarantine.

```sh
python3 -m unittest discover -s tests -p 'test_br_runtime_snapshot.py' -v
```

The two-clone regressions perform real Git untracking/fast-forward transitions
with both clean and dirty peer files, then recover the peer's exact original
bytes into a new directory. Synthetic stock-SQLite databases contain committed
WAL-only rows; the recovered copies reopen with those rows and pass
`integrity_check`, while a main-only control loses the WAL-only rows. Opaque
certificate sentinels prove byte preservation, not FrankenSQLite certificate
validity. These tests never open production trackers and are not native
FrankenSQLite, native `br`, or release qualification.
