# Completed compaction recovery retention (#554)

## Integration and qualification

The implementation is `src/config/compaction_retention.rs`; its integration is
staged in `.github/patches/compaction-retention.patch`. The patch pairs the
production changes in `src/config/mod.rs` with the real CLI regressions in
`tests/e2e_sync_artifacts.rs`. Native qualification and activation of this
revision are pending. A candidate or an applied local patch does not establish
passing tests or active retention on main.

The patch must apply to the reviewed source with plain `git apply --check`,
without repairing hunk counts. Activation requires every gate below on the
source being published. The qualification workflow checks the paired source
hashes and publishes both files atomically only after all four native matrix
rows pass. Publish directly to main without force; an intervening main change
requires reviewing and qualifying the combined source.

## Retention contract

Only a successful installation of the *new compacted database* may issue a
completion receipt. Both the retained source and the installed replacement
must have a clean integrity verdict. Repairing a damaged source does not make
its backup trustworthy enough to displace older recovery history.

Before VACUUM or REINDEX, probe the private compaction source and bind its clean
verdict to stable main-file length and SHA-256. Checkpoint success alone is
insufficient. In both the source and retained family, a WAL must be absent,
empty, or exactly a 32-byte header that parses and passes the engine's header
checksum validation. A rollback journal must be absent or empty. These states
contain no replayable overlay; file length alone never authorizes cleanup.
The retained main must match the exact integrity-verified bytes. Source repair,
changed bytes, or uncertain overlay state leaves the family unreceipted and
preserves existing history.

Candidate adoption, durable installation, reopening, and final database-family
write-authority verification all precede sealing. The reopened storage must
carry the same authority for the same database. Each integrity check must
return exactly one clean `ok` verdict; empty, mixed, corrupt or failed responses
authorize neither a receipt nor pruning. These are structural integrity gates,
not a claim of full logical equivalence. Reopening the old database after an
installation failure is not completion. Retention refusal after a successful
compaction warns and preserves evidence; it never triggers installation rollback.

For each database independently, retain the newest **five** verified completed
families and **all** completed families younger than **seven days**. Also protect
the generation just completed. A deletion candidate must be outside the newest
five AND at least seven days old. This is not a hard five-set or byte cap. At
44 successful compactions per day the recent-history window can retain about
308 sets. Cleanup runs on the next successful compaction, not on a timer.

Before any removal, five newer intact completed families must pass full content
hash verification. A corrupt newer family cannot consume a protected-history
slot and displace a healthy older backup, even when its inode, length and
modification time still match its receipt. Corrupt evidence itself is retained.

A receipt binds the database path, exact generation, completion time, complete
known-sidecar catalog (including recorded absences), and every retained member's
size, SHA-256, device/inode and modification time. The `br.compaction.complete.v3`
receipt attests both the source and installed integrity results. v1 and v2
receipts lack the complete proof and are never automatically upgraded or
pruned. Routes that cannot be represented as UTF-8 without loss receive no
receipt. All present members must match
and no additional same-generation evidence may exist. Unknown, duplicate,
oversized, future-dated, foreign or malformed receipts confer no deletion
permission. Renaming a repository conservatively invalidates old path receipts.

Unreceipted historical `vacuum-*` sets, failed/incomplete compactions, changed or
missing members, unrecognized sidecars and interrupted pruning directories are
**never automatically adopted or deleted**. Existing migration backups are not
affected. Consequently the reporter's pre-existing 12.1 GB is not automatically
reclaimed, and exceptional/uncertain evidence remains unbounded by design.

## Filesystem safety

Recovery directories are created with mode `0700`, independent of umask `002`
or `022`. An existing directory, including an older `0775` root, may be narrowed
only when it belongs to the effective user and is not world-writable. The
change uses a no-follow directory descriptor, removes group/other and special
permission bits, and never adds owner permissions. The final mode, ownership,
inode and named route are checked again. Symlinks, foreign owners, world-write
access and observed replacement cause refusal. This directory migration never
changes child permissions or retroactively authorizes pruning old contents.

The engine can create fresh family members as `0664` under umask `002`. Permission
preparation is limited to the newly copied private compaction source after its
checkpoint, and the current retained family after the installed database passes
integrity. Their parent must already be private to its owner. Every member must
be an owned, ordinary file with one link, no world-write access and a matching
no-follow descriptor. The entire family is checked before permission changes;
retained members must also match the current installation manifest's size and
SHA-256. Descriptor-based changes remove non-owner and special bits without
adding owner permissions. Content hashes, ownership, inode, mode and named route
are checked again and changes synchronized before sealing. Historical discovery
and pruning never change permissions or upgrade an otherwise ineligible family.

Retention requires the live database-family write authority throughout. It
rejects symlinked routes, unsafe ownership or group/world-writable recovery
entries, hard links, directories in place of files, sockets and other special
entries. Known hard-link aliases are rejected before opening them, preserving
SQLite's process-owned POSIX lock discipline.

Operations use retained directory descriptors, no-follow opens and no-replace
renames. All members and the receipt are verified before any move. The receipt
is claimed first into a new private staging directory, the entire family is
staged and synchronized, and all descriptors and names are checked again before
the first unlink. Content hashes and the no-overlay proof are revalidated for
selected families and again after staging; WAL/journal content is checked before
its unlink. Only individually verified local entries are removed, with the
receipt last; no recursive deletion is used. Interrupted/ambiguous staging
is left as evidence and is never automatically resumed. An I/O failure during
delete can leave a partially pruned private set, which future runs preserve.

This implementation enables removal on Linux, Android and Apple targets, where
the project's flagged no-replace rename primitive is supported. Other targets
keep their existing conservative behavior. The checks defend against observed
substitution and accidental aliasing; they are not a claim of atomic deletion
against arbitrary malicious concurrent writers with the same operating-system
identity. Narrowing modes does not revoke previously opened writable handles;
later observed changes still cause refusal. Local receipts are not cryptographic
signatures.

Discovery reads the recovery directory once and indexes all same-generation
names, preserving opaque filename bytes and ambiguous names that mention more
than one generation. This avoids a complete directory rescan for every receipt
on ordinary imports. The index is not deletion authority: selected keepers and
victims still receive fresh namespace and content checks. If no old victim can
exist, the sweep does not hash recent database payloads.

## Regression gates

Rust regressions cover age/count boundaries, corrupt or incomplete receipts,
unhealthy or mismatched source proof, insufficient trustworthy history, altered
members, unsafe filesystem entries, interrupted claims, and failed installation.
They also exercise directory migration, fresh-member permission preparation,
lost or unrelated database authority, and real compaction through the successful
installation hook. A sole trustworthy backup must survive newer unverified
repair families. Positive tests must
actually create receipts and prune eligible old families; synthetic receipt
tests alone cannot establish that production retention works.

The CLI checks distinguish two cases. Eight ordinary clean imports must update
issue data without creating compaction backups
(`.github/tests/compaction-retention-imports.py`). Eight imports that actually
need compaction must produce valid recent receipts and preserve the final data
(`tests/e2e_sync_artifacts.rs`). Canonical temporary fixture roots accommodate
macOS's `/var` symlink without relaxing production route checks.

The required native matrix is:

| Platform | Process umask | Required behavioral coverage |
|---|---|---|
| Linux | `002` | Directory, retention, repeated-import and recovery/authority regressions |
| Linux | `022` | Same coverage |
| macOS | `002` | Same coverage, including canonical temporary paths |
| macOS | `022` | Same coverage, including canonical temporary paths |

Use the pinned Rust toolchain and locked dependencies. In addition to those
behavioral rows, qualification requires `cargo fmt --all -- --check`,
`cargo check --locked --all-targets`,
`cargo clippy --locked --all-targets -- -D warnings`, the native CLI checks,
relevant integration suites, and `cargo test --locked`. Record commands and
results against the exact tested source; unavailable platforms, skipped tests,
or compilation-only checks are not native behavioral passes.
