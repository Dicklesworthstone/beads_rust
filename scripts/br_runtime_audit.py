#!/usr/bin/env python3
"""Read-only Git audit for beads_rust-9uavz (Python 3.10+, Git required).

Audit a repository or a fleet, without opening a database or reading runtime
payloads. HEAD and index are checked independently: adding .gitignore or staging
an untracking does not make an existing clone safe. --incoming-ref optionally
checks an already-local commit before a pull/checkout; this tool never fetches.

Exit codes: 0 = no recognized tracked runtime paths in the requested scope;
1 = unsafe tracked/runtime transitions; 2 = incomplete/unavailable evidence.
This is a diagnostic, NOT a live-family backup or cross-clone migration tool.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path, PurePosixPath
from typing import Iterable

SCHEMA = "br.git-runtime-audit.v1"
OID = re.compile(rb"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")
MODES = {b"100644", b"100755", b"120000", b"160000"}
SIDECAR_SUFFIXES = (
    (b"-wal-cert-head", "certificate_head"), (b"-wal-cert", "certificate"),
    (b"-fsqlite-ns-gate", "namespace"), (b"-fsqlite-ns-use", "namespace"),
    (b".fsqlite-migration-state", "migration_state"),
    (b"-journal", "journal"), (b"-shm", "shared_index"),
    (b"-wal", "wal"), (b"-ns", "namespace"),
)
GUIDANCE = (
    "Do not run git rm --cached and tell other clones to pull. Git can remove "
    "their runtime files when that commit arrives. Stop tracker users in every "
    "affected clone, retain and verify complete local database families outside "
    "Git's worktree, and coordinate preservation across the Git transition. "
    "This audit neither backs up live databases nor authorizes untracking, "
    "sidecar deletion, checkpointing, or certificate quarantine."
)


class AuditFailure(Exception):
    """Evidence could not be obtained; never interpret this as a clean audit."""


def relative_path(value: str) -> str:
    """Accept literal repository-relative paths, never pathspec syntax."""
    path = PurePosixPath(value)
    if (not value or value in {".", ".."} or path.is_absolute()
            or any(part in {"", ".", ".."} for part in value.split("/"))
            or any(part.casefold() == ".git" for part in path.parts)
            or "\\" in value or "\0" in value or ":" in value):
        raise argparse.ArgumentTypeError("use a literal, repository-relative path without .git or traversal")
    return value


def legacy_recovery_suffix(suffix: bytes) -> bool:
    """Recognize a retained filename suffix, not a substring or child path."""
    if b"/" in suffix:
        return False
    if suffix.startswith((b".stale_", b".rebuild_")):
        return True
    for prefix in (b".bad", b".corrupt"):
        if suffix.startswith(prefix):
            tail = suffix[len(prefix):]
            if not tail or tail[:1] in {b"_", b"-", b"."}:
                return True
    return False


def retained_database_suffix(suffix: bytes) -> bool:
    """Bind legacy evidence to the database or one of its known sidecars."""
    return legacy_recovery_suffix(suffix) or any(
        suffix.startswith(sidecar) and legacy_recovery_suffix(suffix[len(sidecar):])
        for sidecar, _ in SIDECAR_SUFFIXES)


def runtime_kind(path: bytes, beads_dirs: tuple[bytes, ...], databases: tuple[bytes, ...]) -> str | None:
    """Classify names only. Runtime contents, links, and live metadata are not read."""
    for database in databases:
        if path == database:
            return "database"
        for suffix, kind in SIDECAR_SUFFIXES:
            if path == database + suffix:
                return kind
    for directory in beads_dirs:
        prefix = directory + b"/"
        if not path.startswith(prefix):
            continue
        parts = path[len(prefix):].split(b"/")
        name = parts[-1]
        # Recovery namespaces contain receipts and arbitrarily named retained
        # payloads. Classifying just the leaf silently loses those files.
        for part in parts:
            if part in {b".br_recovery", b".br_history"}:
                return "recovery" if part == b".br_recovery" else "local_history"
            if part == b".write-waiters.lock":
                return "coordination_lock"
            if part.startswith(b".br-wal-index-"):
                return "shared_index_recovery"
        if name.endswith(b".db"):
            return "database"
        for suffix, kind in SIDECAR_SUFFIXES:
            if name.endswith(suffix):
                stem = name[:-len(suffix)]
                if (stem.endswith((b".db", b".sqlite", b".sqlite3"))
                        or kind in {"namespace", "migration_state"}
                        or (b".vacuum" in stem and kind.startswith("certificate"))):
                    return kind
        if name.endswith((b".sqlite", b".sqlite3")):
            return "database"
        if b".db-wal" in name:
            # Match init's *.db-wal* rule for retained/extended certificates,
            # not just the two currently published certificate suffixes.
            return "wal_sidecar"
        if name.endswith(b".lock"):
            return "coordination_lock"
        if name.endswith(b".tmp"):
            return "temporary"
        if name in {b"sync_base.jsonl", b"sync-state.json", b"last-touched", b"redirect",
                    b"daemon.log", b"daemon.pid", b"bd.sock"}:
            return "local_state"
        if name in {b"beads.base.jsonl", b"beads.base.meta.json", b"beads.left.jsonl",
                    b"beads.left.meta.json", b"beads.right.jsonl", b"beads.right.meta.json"}:
            return "merge_temporary"
        # Older recovery paths retained the database and sidecars beside the
        # live family. Match the same suffix boundaries as br vcs-status, so
        # ordinary names such as beads.db.badger.md remain shared files.
        for extension in (b".db", b".sqlite", b".sqlite3"):
            _, separator, suffix = name.rpartition(extension)
            if separator and retained_database_suffix(suffix):
                return "recovery"
    # Explicit databases may be extensionless and outside the metadata scope.
    # Require their exact basename; a neighboring tracker or child directory
    # is not part of that database family. Existing categories above win.
    for database in databases:
        if path.startswith(database) and retained_database_suffix(path[len(database):]):
            return "recovery"
    return None


@dataclass(frozen=True)
class Entry:
    mode: str
    object_id: str
    stage: int = 0


def records(output: bytes) -> Iterable[bytes]:
    if not output:
        return []
    if not output.endswith(b"\0"):
        raise AuditFailure("truncated_git_output")
    result = output[:-1].split(b"\0")
    if any(not record for record in result):
        raise AuditFailure("malformed_git_output")
    return result


def parse_entries(output: bytes, *, tree: bool) -> dict[bytes, list[Entry]]:
    entries: dict[bytes, list[Entry]] = {}
    for record in records(output):
        metadata, separator, path = record.partition(b"\t")
        fields = metadata.split(b" ")
        if not separator or len(fields) != 3 or not path or path.startswith(b"/"):
            raise AuditFailure("malformed_git_output")
        if any(part in {b"", b".", b".."} for part in path.split(b"/")):
            raise AuditFailure("unsafe_git_path")
        mode = fields[0]
        object_id = fields[2] if tree else fields[1]
        if mode not in MODES or not OID.fullmatch(object_id):
            raise AuditFailure("unsupported_git_entry")
        if tree:
            expected_type = b"commit" if mode == b"160000" else b"blob"
            if fields[1] != expected_type:
                raise AuditFailure("malformed_git_output")
            stage = 0
        else:
            if fields[2] not in {b"0", b"1", b"2", b"3"}:
                raise AuditFailure("malformed_git_output")
            stage = int(fields[2])
        previous = entries.setdefault(path, [])
        if any(item.stage == stage or item.stage == 0 or stage == 0 for item in previous):
            raise AuditFailure("duplicate_git_entry")
        previous.append(Entry(mode.decode("ascii"), object_id.decode("ascii"), stage))
    for values in entries.values():
        values.sort(key=lambda entry: entry.stage)
    return entries


def hardened_environment() -> dict[str, str]:
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith("GIT_")
           and key.upper() not in {"SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", "GCM_INTERACTIVE"}}
    env.update({"GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0",
                "GIT_LITERAL_PATHSPECS": "1", "GIT_NO_LAZY_FETCH": "1",
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_ATTR_NOSYSTEM": "1", "GIT_PAGER": "", "PAGER": "",
                "GCM_INTERACTIVE": "Never", "LC_ALL": "C"})
    return env


class Git:
    """Bound probes as a group. Only fixed read-only commands are used below."""
    def __init__(self, root: Path, timeout: float, max_bytes: int) -> None:
        self.root = root
        self.deadline = time.monotonic() + timeout
        self.max_bytes = max_bytes
        program = shutil.which("git")
        if program is None:
            raise AuditFailure("git_unavailable")
        self.program = program

    def run(self, *args: str) -> tuple[int, bytes]:
        if time.monotonic() >= self.deadline:
            raise AuditFailure("probe_timed_out")
        command = [self.program, "--no-optional-locks", "--no-pager", "--no-replace-objects",
                   "-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false",
                   "-c", f"core.hooksPath={os.devnull}", "-c", "gc.auto=0",
                   "-c", "maintenance.auto=false", "-c", "protocol.allow=never", *args]
        # File captures avoid an inherited pipe keeping the parent hung after
        # Git exits. Captures are outside the worktree; retained bytes are capped.
        capture_dir = Path(tempfile.gettempdir()).resolve()
        if capture_dir == self.root or self.root in capture_dir.parents:
            raise AuditFailure("capture_directory_inside_worktree")
        try:
            with tempfile.TemporaryFile(dir=capture_dir) as out, tempfile.TemporaryFile(dir=capture_dir) as err:
                process = subprocess.Popen(command, cwd=self.root, env=hardened_environment(),
                                           stdin=subprocess.DEVNULL, stdout=out, stderr=err)
                try:
                    while True:
                        if time.monotonic() >= self.deadline:
                            raise AuditFailure("probe_timed_out")
                        if max(os.fstat(out.fileno()).st_size, os.fstat(err.fileno()).st_size) > self.max_bytes:
                            raise AuditFailure("probe_output_limit")
                        status = process.poll()
                        if status is not None:
                            break
                        time.sleep(min(0.01, max(0.0, self.deadline - time.monotonic())))
                    out.seek(0)
                    output = out.read(self.max_bytes + 1)
                    # The process may have written its final stderr between
                    # the last size check and poll() observing its exit.
                    if (len(output) > self.max_bytes
                            or max(os.fstat(out.fileno()).st_size,
                                   os.fstat(err.fileno()).st_size) > self.max_bytes):
                        raise AuditFailure("probe_output_limit")
                    if time.monotonic() >= self.deadline:
                        raise AuditFailure("probe_timed_out")
                    return status, output
                finally:
                    if process.poll() is None:
                        process.kill()
                    process.wait()
        except OSError as error:
            raise AuditFailure("git_probe_io_error") from error

    def required(self, *args: str) -> bytes:
        status, output = self.run(*args)
        if status != 0:
            raise AuditFailure("git_probe_failed")
        return output

    def commit(self, revision: str, *, allow_unborn: bool = False) -> str | None:
        status, output = self.run("rev-parse", "--verify", "--quiet", "--end-of-options", revision + "^{commit}")
        object_id = output.removesuffix(b"\n")
        if status == 0 and OID.fullmatch(object_id):
            return object_id.decode("ascii")
        if allow_unborn and status == 1:
            code, symbolic = self.run("symbolic-ref", "--quiet", "HEAD")
            branch = symbolic.removesuffix(b"\n")
            if code == 0 and branch.startswith(b"refs/heads/"):
                exists, _ = self.run("show-ref", "--verify", "--quiet", os.fsdecode(branch))
                if exists == 1:
                    return None
        raise AuditFailure("revision_unavailable")


def audit_repository(repo: Path, *, beads_dirs: tuple[str, ...] = (".beads",),
                     databases: tuple[str, ...] = (), incoming_ref: str | None = None,
                     timeout: float = 5.0, max_bytes: int = 8 * 1024 * 1024) -> dict:
    report: dict = {"repository": str(repo), "status": "unavailable", "complete": False,
                    "findings": [], "head": None, "incoming_commit": None,
                    "scope": {"beads_directories": list(beads_dirs), "databases": list(databases)},
                    "observation_atomic": False, "repair_supported": False}
    try:
        for value in (*beads_dirs, *databases):
            relative_path(value)
        root = repo.resolve(strict=True)
        git = Git(root, timeout, max_bytes)
        if git.required("rev-parse", "--is-inside-work-tree") != b"true\n":
            raise AuditFailure("not_worktree")
        # An explicit root prevents relative scope from silently auditing a
        # different directory when the caller supplied a nested path.
        if git.required("rev-parse", "--show-prefix") != b"\n":
            raise AuditFailure("repository_root_required")
        head = git.commit("HEAD", allow_unborn=True)
        incoming = git.commit(incoming_ref) if incoming_ref is not None else None
        # Include ancestor entries: a pathspec beneath a gitlink or symlink
        # yields an empty listing, not evidence of an untracked directory.
        # Scan each top-level scope under the same byte/time caps, then filter
        # findings to the exact requested paths below.
        scopes = {directory.split("/", 1)[0] for directory in beads_dirs}
        for database in databases:
            scopes.add(database.split("/", 1)[0] if "/" in database else ".")
        pathspecs = tuple(sorted(scopes))
        index_args = ("ls-files", "--stage", "--full-name", "-z", "--", *pathspecs)
        index_bytes = git.required(*index_args)
        index = parse_entries(index_bytes, tree=False)

        def tree(commit: str | None) -> dict[bytes, list[Entry]]:
            if commit is None:
                return {}
            return parse_entries(git.required("ls-tree", "-r", "--full-tree", "-z", commit, "--", *pathspecs), tree=True)

        head_paths = tree(head)
        incoming_paths = tree(incoming)
        encoded_dirs = tuple(os.fsencode(value) for value in beads_dirs)
        encoded_databases = tuple(os.fsencode(value) for value in databases)
        for path in head_paths.keys() | index.keys() | incoming_paths.keys():
            # Git does not recurse into gitlinks or follow symlinked tracker
            # directories. Their empty descendant listing is not clean evidence.
            if path in encoded_dirs or any(
                    requested.startswith(path + b"/")
                    for requested in (*encoded_dirs, *encoded_databases)):
                raise AuditFailure("scope_crosses_tracked_non_directory")
        # These consistency checks do not claim an atomic Git snapshot. They
        # catch observed races and fail closed rather than emit a false clean.
        if git.required(*index_args) != index_bytes or git.commit("HEAD", allow_unborn=True) != head:
            raise AuditFailure("repository_changed_during_audit")
        if incoming_ref is not None and git.commit(incoming_ref) != incoming:
            raise AuditFailure("incoming_ref_changed_during_audit")
        findings = []
        for path in sorted(head_paths.keys() | index.keys() | incoming_paths.keys()):
            kind = runtime_kind(path, encoded_dirs, encoded_databases)
            if kind is None:
                continue
            before, staged, after = head_paths.get(path, []), index.get(path, []), incoming_paths.get(path, [])
            findings.append({"path": os.fsdecode(path), "path_bytes_hex": path.hex(), "kind": kind,
                             "head_entries": [asdict(item) for item in before],
                             "index_entries": [asdict(item) for item in staged],
                             "incoming_entries": [asdict(item) for item in after],
                             "staged_untracking": bool(before and not staged),
                             "unmerged": any(item.stage != 0 for item in staged),
                             "incoming_deletes": bool(incoming and before and not after),
                             "incoming_replaces": bool(incoming and before and after and before != after),
                             "incoming_adds": bool(incoming and after and not before)})
        report.update(repository=str(root), status="unsafe" if findings else "clean", complete=True,
                      findings=findings, head=head, incoming_commit=incoming)
    except (AuditFailure, OSError, argparse.ArgumentTypeError) as error:
        report["reason"] = str(error) if isinstance(error, AuditFailure) else "invalid_repository_or_scope"
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("repositories", nargs="*", type=Path, default=[Path.cwd()], help="worktree roots; defaults to current directory")
    parser.add_argument("--beads-dir", action="append", type=relative_path, dest="beads_dirs", help="repository-relative tracker directory; repeatable (default .beads)")
    parser.add_argument("--database", action="append", type=relative_path, default=[], help="explicit repository-relative database name, including extensionless/custom paths")
    parser.add_argument("--incoming-ref", help="already-local commit/ref to inspect; no fetch or checkout")
    parser.add_argument("--timeout", type=float, default=5.0, help="seconds per repository, 0.1..60 (default 5)")
    parser.add_argument("--max-output-bytes", type=int, default=8 * 1024 * 1024, help="per-stream capture limit, 1024..67108864")
    parser.add_argument("--json", action="store_true", help="machine-readable fleet report with lossless path bytes")
    args = parser.parse_args(argv)
    if not 0.1 <= args.timeout <= 60 or not 1024 <= args.max_output_bytes <= 64 * 1024 * 1024:
        parser.error("timeout or output bound is outside the supported range")
    reports = [audit_repository(repo, beads_dirs=tuple(args.beads_dirs or [".beads"]),
                               databases=tuple(args.database), incoming_ref=args.incoming_ref,
                               timeout=args.timeout, max_bytes=args.max_output_bytes)
               for repo in args.repositories]
    code = 2 if any(not report["complete"] for report in reports) else int(any(report["findings"] for report in reports))
    result = {"schema": SCHEMA, "exit_code": code, "repositories": reports, "guidance": GUIDANCE,
              "limits": "Recognized names in explicitly selected Git scopes only; no live payload, untracked-file, backup, or cross-clone safety verification."}
    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=True))
    else:
        for report in reports:
            print(f'{report["status"]}: {json.dumps(report["repository"], ensure_ascii=True)}')
            if not report["complete"]:
                print(f'  evidence unavailable: {report["reason"]}')
            for finding in report["findings"]:
                changes = [name for name in ("staged_untracking", "unmerged", "incoming_deletes", "incoming_replaces", "incoming_adds") if finding[name]]
                print(f'  {json.dumps(finding["path"], ensure_ascii=True)} [{finding["kind"]}] {", ".join(changes)}')
        if code:
            print(GUIDANCE)
    return code


if __name__ == "__main__":
    raise SystemExit(main())
