---
name: Bug report
about: Something br did wrong, with the evidence that lets us localize it
title: ''
labels: bug
assignees: ''
---

## What happened

<!-- The command you ran, what you expected, what you got. Paste exact stdout and stderr in the block below. -->

```text
$ br ...
```

## Version and platform

```text
$ br --version
```

OS / filesystem (for example: Ubuntu 24.04 on ext4, macOS 15 on APFS, WSL2 on a DrvFS mount, Windows 11 NTFS):

## Selftest receipt

The selftest drives this exact binary through a full issue lifecycle in a
throwaway directory on your filesystem and prints a receipt. It never touches
your workspace. Paste its output (it fails fast when a platform or filesystem
problem is the cause):

```text
$ RUST_LOG=error br doctor --selftest --json
```

## Incident bundle (workspace problems: sync, recovery, corruption, doctor findings)

Run this in the affected repository and attach the archive. It contains the
doctor/health/sync/where output, directory listings, database-family hashes,
the metadata and schema tables, recent events, and redacted copies of
`metadata.json` / `config.yaml`. Database bytes and `issues.jsonl` are only
included if you add `--include-db` / `--include-jsonl`; e-mail addresses are
redacted in every text member.

```text
$ br doctor --bundle /tmp/incident-$(date +%Y%m%d-%H%M%S).tar.gz --json
```

## Anything else

<!-- Timeline (last successful operation, the operation that failed), concurrent agents or processes, custom policy.yaml or config.yaml. -->
