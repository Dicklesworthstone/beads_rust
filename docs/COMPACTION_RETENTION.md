# Completed compaction recovery retention (#554)

## Integration and qualification

The implementation is `src/config/compaction_retention.rs`. The hand-authored
hook in `.github/patches/compaction-retention.patch` targets the reviewed config
blob `fcb95f51ee2dab19b26f7f7ba9c3a192f439d54e`. The qualification workflow applies
that hook to the exact triggering main revision and activates it in a separate,
non-forced main commit only after every validation gate passes. Until that
activation commit exists, the implementation is not wired into compaction.
Do not infer passing tests or a deployed fix from the candidate commit alone.

## Retention contract

Only a successful installation of the *new compacted database* may issue a
completion receipt. Candidate adoption, durable installation, reopening, and
final database-family write-authority verification all precede sealing. An
installation failure that successfully reopens the old database is not a
completed compaction. A retention error after success warns and preserves
evidence; it never sends that successful installation into rollback.

For each database independently, retain the newest **five** verified completed
families and **all** completed families younger than **seven days**. Also protect
the generation just completed. A deletion candidate must be outside the newest
five AND at least seven days old. This is not a hard five-set or byte cap. At
44 successful compactions per day the recent-history window can retain about
308 sets. Cleanup runs on the next successful compaction, not on a timer.

A receipt binds the database path, exact generation, completion time, complete
known-sidecar catalog (including recorded absences), and every retained member's
size, SHA-256, device/inode and modification time. All present members must match
and no additional same-generation evidence may exist. Unknown, duplicate,
oversized, future-dated, foreign or malformed receipts confer no deletion
permission. Renaming a repository conservatively invalidates old path receipts.

Unreceipted historical `vacuum-*` sets, failed/incomplete compactions, changed or
missing members, unrecognized sidecars and interrupted pruning directories are
**never automatically adopted or deleted**. Existing migration backups are not
affected. Consequently the reporter's pre-existing 12.1 GB is not automatically
reclaimed, and exceptional/uncertain evidence remains unbounded by design.

## Filesystem safety

Retention requires the live database-family write authority throughout. It
rejects symlinked routes, unsafe ownership or group/world-writable recovery
entries, hard links, directories in place of files, sockets and other special
entries. Known hard-link aliases are rejected before opening them, preserving
SQLite's process-owned POSIX lock discipline.

Operations use retained directory descriptors, no-follow opens and no-replace
renames. All members and the receipt are verified before any move. The receipt
is claimed first into a new private staging directory, the entire family is
staged and synchronized, and all descriptors and names are checked again before
the first unlink. Only individually verified local entries are removed, with
the receipt last; no recursive deletion is used. Interrupted/ambiguous staging
is left as evidence and is never automatically resumed. An I/O failure during
delete can leave a partially pruned private set, which future runs preserve.

This implementation enables removal on Linux, Android and Apple targets, where
the project's flagged no-replace rename primitive is supported. Other targets
keep their existing conservative behavior. The checks defend against observed
substitution and accidental aliasing; they are not a claim of atomic deletion
against arbitrary malicious concurrent writers with the same operating-system
identity, nor are local receipts cryptographic signatures.

## Regression gates

Fifteen Rust tests cover count/age boundaries, frequent compactions, legacy and
partial receipts, missing/changed/extra family members, inode replacement,
malformed proof, independent databases, unsafe entries, directory substitution,
interrupted staging, staging collisions, real repeated compaction, failed
installation phases, and nonfatal cleanup refusal. The CLI regression imports
eight changed JSONL revisions and verifies completion proof, recent-family
preservation and the final issue data.

Qualification runs the pinned toolchain with the locked dependency graph:
retention tests, `cargo check --all-targets`, `cargo clippy --all-targets -- -D
warnings`, repeated CLI imports, `cargo fmt --all -- --check`, and `cargo test`.
The workflow uses no external Actions and supplies its write token only to the
final publication step. It refuses publication if main advanced, an unexpected
file changed, any gate failed, or uploaded bytes differ from the tested source.
