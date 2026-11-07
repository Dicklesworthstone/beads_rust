# Official v0.7.6 workflow database fixture (GitHub #596)

`schema19_v076_workflows.db.gz` is a real database created with the official
Linux amd64 **v0.7.6 release executable**, not a synthetic DDL fixture or a
newer database stamped with an old version. The source was a new disposable
workspace. No production tracker was opened or changed. It supplements the
separate `schema19_v076_release.db.gz` fixture landed upstream; neither fixture
replaces or changes the other.

## Artifact identity

- Release: <https://github.com/Dicklesworthstone/beads_rust/releases/tag/v0.7.6>
- Archive: <https://github.com/Dicklesworthstone/beads_rust/releases/download/v0.7.6/br-0.7.6-linux_amd64.tar.gz>
- Archive SHA-256:
  `56baae2df5e7cc6f6537e05fd5f560192a9d5ceb02659c61b10b9610b45eb3ef`
- Extracted executable SHA-256:
  `73cbef7f11680b8349ef5b465d002e404272cc6c06b0f95a86035b488b1844b9`
- `br version --json` identifies version `0.7.6`, release build, commit
  `ad4212f562ca59864946b8620c1361f0933ee4a8`, Rust `1.100.0-nightly`, target
  `x86_64-unknown-linux-gnu`, features `["self_update"]`.
- Uncompressed database SHA-256:
  `0319880b8e7f0cdaba60ded45936bd6613be0aa1b5a2993c7567f541c8cbdc8b`
- Gzip SHA-256:
  `0f5a0d50ed916dd94e273e90ba8e237bc1bd3d31b2a1a53192ddca3ccf5435fc`

The official archive digest was compared with the release asset's GitHub
metadata before running the executable. The database reports `user_version=19`
and stock SQLite `quick_check=ok`. The final release command checkpointed the
database and left a 32-byte, frame-free WAL. Only the checkpointed main file is
compressed, using gzip with `mtime=0`; no journal or certificate sidecar is
required to recover its rows.

## Content and authority

The fixture contains four issues, including one ephemeral issue omitted from
the three-line companion `schema19_v076_workflows_issues.jsonl`. It also contains:

| Table | Rows |
|---|---:|
| `events` | 23 |
| `dependencies` | 3 |
| `comments` | 1 |
| `labels` | 2 |
| `close_metadata` | 1 |
| `gate_results` | 1 |
| `gate_result_history` | 2 |
| `capacity_exemptions` | 1 |
| `capacity_exemption_history` | 2 |
| `capacity_occupancy` | 4 |
| `child_counters` | 1 |
| `config` | 1 |
| `metadata` | 12 |
| `sqlite_sequence` | 4 |
| `export_hashes` | 3 |
| `blocked_issues_cache` | 1 |
| `dirty_issues` | 0 |

The two dependencies from the same issue to the same target have distinct
`blocks` and `related` types and independent JSON payloads. Closure metadata
contains actual release attribution and a policy-bypass reason. Gate history
contains both a failed and passing verdict. Capacity history contains a grant
and renewal, while occupancy includes the actor, agent, and harness.

`close_metadata` has the release's eight columns and **no `closed_by`**.
`jsonl_row_receipts` is absent. These are the runtime differences reported in
#596 even though release and pre-fix main both called the schema version 19.

The integration test separately supplements this genuine release database
with non-default database configuration, operator metadata, an operator table,
index and view, duplicate BLOB rows with nonconsecutive rowids, dependency
provenance, and high-water allocation reservations. Those are test additions,
not claims about records emitted by the release binary. The test compares
every original column with its SQLite storage class, multiplicity, hidden
application/operator rowid, and `sqlite_sequence` names/high-water values after
apply, reconstructed interruption states, undo, and repeat apply. The direct
`heal` case withholds the JSONL file entirely. Existing release-fixture tests
cover the three combinations with missing runtime additions and the fully
upgraded no-op layout. New plans bind `schema19_preservation` with profile
`schema19-layout-preservation-v2` and a source digest covering row identity as
well as exact stored values and operator schema. Rowid-only edits and changes
between distinct non-UTF-8 TEXT payloads must invalidate the plan. The raw-TEXT
regression also checks refusal of undo after newer database-only edits.

The interruption cases reconstruct two persisted boundaries from a genuine
completed migration and retain all displaced test artifacts: installation
before `applied.json` publication, and a commit-ready intent whose original
database is still live. They assert that ordinary reads do not finish either
operation, that explicit apply resumes or retries, and that the original
backup stays intact. These are deterministic state-recovery tests, not
abrupt process termination, power-loss, or filesystem fault-injection tests.

## Generation recipe

Every command below used the checksum-verified release executable and global
flags `--actor release-fixture --json --no-auto-import --no-auto-flush`.
`BEADS_DIR` and `BEADS_DB` were explicitly bound to the disposable tracker.
Generated issue IDs in this fixture were `release-0ta`, `release-0ta.1`,
`release-66o`, and `release-9sp`; another generation will have different IDs and
timestamps. The variables below represent the corresponding returned IDs.

```bash
br init --prefix release
br create 'Release upgrade parent' --type epic \
  --description 'Retain parent, relation metadata, comments and audit history.'
# parent = returned ID
br create 'Release upgrade closed child' --parent "$parent" \
  --labels migration,release --acceptance '- [x] preserve every authoritative row'
# child = returned ID
br create 'Release upgrade dependent' \
  --description 'Related and blocking edges must survive independently.'
# dependent = returned ID
br create 'Database-only ephemeral issue' --ephemeral
br dep add "$dependent" "$parent" --type blocks \
  --metadata '{"source":"official-release","must_preserve":true}'
br dep add "$dependent" "$parent" --type related \
  --metadata '{"independent_parallel_relation":true}'
br comments add "$child" $'Release-created comment: unicode Ω, newline\nsecond line'
br update "$child" \
  --notes 'Database history remains authoritative after JSONL export.' \
  --design 'Preserve every original column and hidden identity.'
br close "$child" --reason 'Release closure proof' \
  --agent-name release-agent --harness release-harness \
  --model release-model --session release-session
br config set compact_model release-private-model
br config set compact_batch_size 37
br config set compaction_enabled false
```

Install the `workflow` section of `schema19_v076_workflows_policy.yaml`, then:

```bash
br gate report "$parent" --gate ci_green --provider ci --status fail \
  --to in_progress --note 'First release verdict'
br gate report "$parent" --gate ci_green --provider ci --status pass \
  --to in_progress --note 'Second release verdict'
br capacity exempt "$parent" --status in_progress --provider operator \
  --reason 'Release preservation witness' --expires 2099-12-31
br capacity renew "$parent" --status in_progress --provider operator \
  --reason 'Renewal must survive' --expires 2099-12-30
br update "$parent" --status in_progress \
  --agent-name release-agent --harness release-harness
br sync --flush-only
br stats
br reopen "$child"
br close "$child" --reason 'Release closure proof with workflow policy' \
  --bypass-policy --bypass-reason 'Retain explicit policy bypass audit' \
  --agent-name release-agent --harness release-harness \
  --model release-model --session release-session
br sync --flush-only
```

Append the `close_policy` section of `schema19_v076_workflows_policy.yaml`, then:

```bash
br reopen "$child"
br close "$child" --reason 'Release closure proof' \
  --bypass-policy --bypass-reason 'Retain explicit policy bypass audit' \
  --agent-name release-agent --harness release-harness \
  --model release-model --session release-session
br sync --flush-only
```

The release writes `br config set` values to YAML, so the companion config
file holds those three settings. The test's database-only config rows are
added explicitly to exercise the preservation contract independently of that
file.

## Qualification commands

The fixture-driven tests have no download prerequisite:

```bash
cargo test --test e2e_schema_migration_upgrade schema19
```

The explicit executable-downgrade check is an additional ignored test so a
normal test run cannot silently fetch and execute a release asset. After
verifying the archive and binary identities above:

```bash
BR_V076_BINARY=/absolute/path/to/verified/br \
  cargo test --test e2e_schema_migration_upgrade \
  official_v076_refuses_upgraded_schema19_layout_and_reads_guarded_undo -- --ignored
```

The ignored test itself checks the executable's SHA-256 and release metadata.
Both layouts retain `user_version=19`; the old binary must refuse the actual
upgraded `close_metadata` layout, not a synthetic future-version header.
Every invocation of the old executable uses a disposable copy of the current
database family. Older executables can create or modify sidecars even when a
read fails, so they must not be probed against the live upgraded tracker.
The actual migrated family remains untouched by that probe. Guarded `undo`
then restores the byte-exact source; another disposable copy must be readable
by v0.7.6 and report all four issues. This does not claim that replacing the
executable alone is safe, or that undo is permitted after new tracker writes.
