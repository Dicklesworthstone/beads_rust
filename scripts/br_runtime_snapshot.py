#!/usr/bin/env python3
"""Offline directory preservation for tracker runtime families (Python 3.10+, POSIX).

Capture ALL regular files and empty directories, not a guessed sidecar subset.
Stop every tracker user before capture. --writers-stopped is the operator's
assertion, NOT an acquired engine lock or an online-snapshot guarantee. Sources
are never opened through SQLite, changed, checkpointed, or followed through links.
Bundles must be new directories outside Git worktrees. No Git operation runs.

Keep the returned manifest_sha256 separately: verification requires that trusted
receipt, not a checksum supplied by the bundle itself. Failures retain incomplete
output for inspection; no existing path is overwritten or deleted.
"""
from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import sys
import time

SCHEMA = "br.runtime-snapshot.v1"
MANIFEST_LIMIT = 32 * 1024 * 1024
MAX_ENTRIES = 100_000
MAX_DEPTH = 64
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
CHUNK = 1024 * 1024


class SnapshotFailure(Exception):
    """Refusal or incomplete preservation, never a successful snapshot."""


class Budget:
    def __init__(self, timeout: float, max_bytes: int) -> None:
        if not math.isfinite(timeout) or not 0 < timeout <= 3600:
            raise SnapshotFailure("invalid_timeout")
        if type(max_bytes) is not int or not 0 <= max_bytes <= 1 << 40:
            raise SnapshotFailure("invalid_byte_limit")
        self.deadline = time.monotonic() + timeout
        self.max_bytes = max_bytes

    def check(self) -> None:
        if time.monotonic() >= self.deadline:
            raise SnapshotFailure("deadline_exceeded")


def platform_check() -> None:
    if (os.name != "posix" or not hasattr(os, "O_NOFOLLOW")
            or not {os.open, os.stat, os.mkdir}.issubset(os.supports_dir_fd)
            or os.scandir not in os.supports_fd):
        raise SnapshotFailure("descriptor_relative_filesystem_support_required")


def absolute(path: Path) -> Path:
    if ".." in path.parts:
        raise SnapshotFailure("parent_traversal_refused")
    if path.anchor == "//":
        raise SnapshotFailure("ambiguous_double_slash_root")
    return Path(os.path.abspath(path))


@contextmanager
def directory(path: Path):
    """Open every component without following a symlink, including ancestors."""
    path = absolute(path)
    fd = os.open(path.anchor, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in path.parts[1:]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


@contextmanager
def member(root: int, path: bytes, *, is_directory: bool = False):
    parts = path.split(b"/") if path else []
    fd = os.dup(root)
    try:
        for index, part in enumerate(parts):
            flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            if is_directory or index < len(parts) - 1:
                flags |= os.O_DIRECTORY
            child = os.open(part, flags, dir_fd=fd)
            os.close(fd)
            fd = child
        yield fd
    finally:
        os.close(fd)


def identity(info: os.stat_result) -> tuple[int, ...]:
    # Access time may change when read. Content-relevant metadata must not.
    return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink,
            info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def assert_bound(path: Path, fd: int) -> None:
    with directory(path) as current:
        if identity(os.fstat(current)) != identity(os.fstat(fd)):
            raise SnapshotFailure("directory_identity_changed")


def directory_names(fd: int, budget: Budget, maximum: int) -> list[bytes]:
    names = []
    with os.scandir(fd) as scan:
        for entry in scan:
            budget.check()
            if len(names) >= maximum:
                raise SnapshotFailure("inventory_limit_exceeded")
            names.append(os.fsencode(entry.name))
    return sorted(names)


def inventory(root: int, budget: Budget) -> dict[bytes, tuple[str, tuple[int, ...]]]:
    result: dict[bytes, tuple[str, tuple[int, ...]]] = {}
    device = os.fstat(root).st_dev
    total = 0

    def walk(fd: int, prefix: bytes, depth: int) -> None:
        nonlocal total
        budget.check()
        if depth > MAX_DEPTH or len(result) >= MAX_ENTRIES:
            raise SnapshotFailure("inventory_limit_exceeded")
        result[prefix] = ("directory", identity(os.fstat(fd)))
        for name in directory_names(fd, budget, MAX_ENTRIES - len(result)):
            budget.check()
            if depth >= MAX_DEPTH:
                raise SnapshotFailure("inventory_limit_exceeded")
            if name.lower() == b".git":
                raise SnapshotFailure("git_metadata_refused")
            if len(result) >= MAX_ENTRIES:
                raise SnapshotFailure("inventory_limit_exceeded")
            path = prefix + b"/" + name if prefix else name
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if info.st_dev != device:
                raise SnapshotFailure("mounted_subtree_refused")
            if stat.S_ISDIR(info.st_mode):
                with member(fd, name, is_directory=True) as child:
                    if identity(os.fstat(child)) != identity(info):
                        raise SnapshotFailure("source_changed")
                    walk(child, path, depth + 1)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                total += info.st_size
                if total > budget.max_bytes:
                    raise SnapshotFailure("byte_limit_exceeded")
                result[path] = ("file", identity(info))
            else:
                raise SnapshotFailure("symlink_hardlink_or_special_file_refused")

    walk(root, b"", 0)
    return dict(sorted(result.items()))


def write_all(fd: int, data: bytes) -> None:
    remaining = memoryview(data)
    while remaining:
        count = os.write(fd, remaining)
        if count <= 0:
            raise SnapshotFailure("short_write")
        remaining = remaining[count:]


def stream(fd: int, size: int, budget: Budget, destination: int | None = None) -> str:
    before = identity(os.fstat(fd))
    if not stat.S_ISREG(before[2]) or before[3] != 1 or before[4] != size:
        raise SnapshotFailure("file_identity_or_size_changed")
    os.lseek(fd, 0, os.SEEK_SET)
    digest = hashlib.sha256()
    remaining = size
    while remaining:
        budget.check()
        data = os.read(fd, min(CHUNK, remaining))
        if not data:
            raise SnapshotFailure("short_read")
        digest.update(data)
        if destination is not None:
            write_all(destination, data)
        remaining -= len(data)
    budget.check()
    if os.read(fd, 1) or identity(os.fstat(fd)) != before:
        raise SnapshotFailure("file_changed_during_read")
    return digest.hexdigest()


@contextmanager
def new_file(parent: int, name: bytes):
    fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
    try:
        os.fchmod(fd, 0o600)
        yield fd
        os.fsync(fd)
    finally:
        os.close(fd)


@contextmanager
def external_output(path: Path, excluded: Path, *also_excluded: Path):
    """Create once outside worktrees; never repurpose an existing directory."""
    path = absolute(path)
    exclusions = [absolute(item) for item in (excluded, *also_excluded)]
    protected = set()
    for item in exclusions:
        if path == item or item in path.parents or path in item.parents:
            raise SnapshotFailure("source_and_output_overlap")
        try:
            with directory(item) as original:
                info = os.fstat(original)
                protected.add((info.st_dev, info.st_ino))
        except FileNotFoundError:
            # The recorded original may be gone after a Git transition.
            continue
    with directory(path.parent) as parent:
        for ancestor in (path.parent, *path.parent.parents):
            with directory(ancestor) as fd:
                info = os.fstat(fd)
                if (info.st_dev, info.st_ino) in protected:
                    raise SnapshotFailure("source_and_output_overlap")
                try:
                    os.stat(b".git", dir_fd=fd, follow_symlinks=False)
                except FileNotFoundError:
                    continue
                raise SnapshotFailure("output_inside_git_worktree")
        os.mkdir(path.name, 0o700, dir_fd=parent)
        with member(parent, os.fsencode(path.name), is_directory=True) as output:
            os.fchmod(output, 0o700)
            yield output
            assert_bound(path, output)
            os.fsync(output)
            os.fsync(parent)


def encode_manifest(value: dict) -> bytes:
    encoded = (json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")) + "\n").encode()
    if len(encoded) > MANIFEST_LIMIT:
        raise SnapshotFailure("manifest_limit_exceeded")
    return encoded


def capture(source: Path, bundle: Path, *, writers_stopped: bool = False,
            timeout: float = 300, max_bytes: int = 4 * 1024**3) -> dict:
    platform_check()
    if not writers_stopped:
        raise SnapshotFailure("stop_all_tracker_users_and_pass_writers_stopped")
    budget = Budget(timeout, max_bytes)
    source, bundle = absolute(source), absolute(bundle)
    with directory(source) as root:
        before = inventory(root, budget)
        with external_output(bundle, source) as output:
            os.mkdir(b"payload", 0o700, dir_fd=output)
            entries = []
            with member(output, b"payload", is_directory=True) as payload:
                for path, (kind, witness) in before.items():
                    budget.check()
                    record = {"path_hex": path.hex(), "kind": kind, "mode": witness[2] & 0o777}
                    if kind == "file":
                        object_name = f"{len(entries):08d}"
                        with member(root, path) as live, new_file(payload, object_name.encode()) as saved:
                            if identity(os.fstat(live)) != witness:
                                raise SnapshotFailure("source_changed")
                            digest = stream(live, witness[4], budget, saved)
                            os.fsync(saved)
                            if stream(saved, witness[4], budget) != digest:
                                raise SnapshotFailure("backup_readback_mismatch")
                        record.update(object=object_name, size=witness[4], sha256=digest)
                    entries.append(record)
                # Re-read every source file, then compare the complete namespace.
                # This detects observed drift, but does not replace quiescence.
                for record in entries:
                    if record["kind"] == "file":
                        path = bytes.fromhex(record["path_hex"])
                        with member(root, path) as live:
                            if (identity(os.fstat(live)) != before[path][1]
                                    or stream(live, record["size"], budget) != record["sha256"]):
                                raise SnapshotFailure("source_changed")
                if inventory(root, budget) != before:
                    raise SnapshotFailure("source_changed")
                assert_bound(source, root)
                os.fsync(payload)
            manifest = {"schema": SCHEMA, "source_path_hex": os.fsencode(source).hex(),
                        "quiescence": "operator_asserted", "entries": entries}
            raw = encode_manifest(manifest)
            budget.check()
            # The manifest is the completion marker and is always written last.
            with new_file(output, b"manifest.json") as saved:
                write_all(saved, raw)
    return {"status": "captured", "bundle": str(bundle),
            "manifest_sha256": hashlib.sha256(raw).hexdigest(),
            "files": sum(record["kind"] == "file" for record in entries),
            "bytes": sum(record.get("size", 0) for record in entries),
            "quiescence": "operator_asserted", "engine_validated": False}


def unique_members(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise SnapshotFailure("duplicate_manifest_member")
        result[key] = value
    return result


def parse_manifest(raw: bytes, expected: str, budget: Budget) -> tuple[list[dict], Path]:
    if not isinstance(expected, str) or not DIGEST.fullmatch(expected):
        raise SnapshotFailure("trusted_manifest_sha256_required")
    if hashlib.sha256(raw).hexdigest() != expected:
        raise SnapshotFailure("manifest_digest_mismatch")
    try:
        document = json.loads(raw, object_pairs_hook=unique_members)
        if (set(document) != {"schema", "source_path_hex", "quiescence", "entries"}
                or document["schema"] != SCHEMA or document["quiescence"] != "operator_asserted"
                or not isinstance(document["source_path_hex"], str)
                or not bytes.fromhex(document["source_path_hex"]).startswith(b"/")):
            raise SnapshotFailure("invalid_manifest")
        entries = document["entries"]
        if type(entries) is not list or not 1 <= len(entries) <= MAX_ENTRIES:
            raise SnapshotFailure("invalid_manifest")
        seen: dict[bytes, str] = {}
        total = 0
        for index, record in enumerate(entries):
            budget.check()
            kind = record["kind"]
            required = {"path_hex", "kind", "mode"}
            if kind == "file":
                required |= {"object", "size", "sha256"}
            if kind not in {"file", "directory"} or set(record) != required:
                raise SnapshotFailure("invalid_manifest")
            path = bytes.fromhex(record["path_hex"])
            parts = path.split(b"/") if path else []
            if (record["path_hex"] != path.hex() or b"\0" in path
                    or len(parts) > MAX_DEPTH
                    or any(part in {b"", b".", b".."} or part.lower() == b".git" for part in parts)
                    or path in seen or (index == 0 and (path or kind != "directory"))
                    or (index > 0 and seen.get(path.rpartition(b"/")[0]) != "directory")):
                raise SnapshotFailure("unsafe_manifest_path")
            if type(record["mode"]) is not int or not 0 <= record["mode"] <= 0o777:
                raise SnapshotFailure("invalid_manifest")
            if kind == "file":
                if (record["object"] != f"{index:08d}" or type(record["size"]) is not int
                        or record["size"] < 0 or not DIGEST.fullmatch(record["sha256"])):
                    raise SnapshotFailure("invalid_manifest")
                total += record["size"]
                if total > budget.max_bytes:
                    raise SnapshotFailure("byte_limit_exceeded")
            seen[path] = kind
        source = Path(os.fsdecode(bytes.fromhex(document["source_path_hex"])))
        return entries, absolute(source)
    except (KeyError, TypeError, ValueError, RecursionError) as error:
        raise SnapshotFailure("invalid_manifest") from error


@contextmanager
def verified_bundle(bundle: Path, expected: str, budget: Budget):
    with directory(bundle) as root:
        before = identity(os.fstat(root))
        if set(directory_names(root, budget, 2)) != {b"manifest.json", b"payload"}:
            raise SnapshotFailure("incomplete_or_unexpected_bundle_members")
        with member(root, b"manifest.json") as file:
            info = os.fstat(file)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MANIFEST_LIMIT:
                raise SnapshotFailure("invalid_manifest_file")
            with os.fdopen(os.dup(file), "rb") as reader:
                raw = reader.read(MANIFEST_LIMIT + 1)
            if identity(os.fstat(file)) != identity(info):
                raise SnapshotFailure("manifest_changed")
        entries, source = parse_manifest(raw, expected, budget)
        with member(root, b"payload", is_directory=True) as payload:
            payload_before = identity(os.fstat(payload))
            objects = {record["object"] for record in entries if record["kind"] == "file"}
            if set(directory_names(payload, budget, len(objects))) != {name.encode() for name in objects}:
                raise SnapshotFailure("payload_inventory_mismatch")
            for record in entries:
                if record["kind"] == "file":
                    with member(payload, record["object"].encode()) as saved:
                        if stream(saved, record["size"], budget) != record["sha256"]:
                            raise SnapshotFailure("payload_digest_mismatch")
            yield entries, payload, source
            budget.check()
            if identity(os.fstat(root)) != before or identity(os.fstat(payload)) != payload_before:
                raise SnapshotFailure("bundle_changed")
        assert_bound(absolute(bundle), root)


def verify(bundle: Path, expected: str, *, timeout: float = 300,
           max_bytes: int = 4 * 1024**3) -> dict:
    platform_check()
    with verified_bundle(bundle, expected, Budget(timeout, max_bytes)) as (entries, _, _source):
        result = {"status": "verified", "manifest_sha256": expected,
                  "files": sum(record["kind"] == "file" for record in entries),
                  "bytes": sum(record.get("size", 0) for record in entries),
                  "engine_validated": False}
    return result


def verify_materialized(data: int, entries: list[dict], budget: Budget) -> None:
    before = inventory(data, budget)
    expected_paths = {bytes.fromhex(record["path_hex"]): record for record in entries}
    if before.keys() != expected_paths.keys():
        raise SnapshotFailure("materialized_inventory_mismatch")
    for path, record in expected_paths.items():
        if before[path][0] != record["kind"]:
            raise SnapshotFailure("materialized_kind_mismatch")
        if record["kind"] == "file":
            with member(data, path) as file:
                if stream(file, record["size"], budget) != record["sha256"]:
                    raise SnapshotFailure("materialized_digest_mismatch")
    if inventory(data, budget) != before:
        raise SnapshotFailure("materialized_directory_changed")


def materialize(bundle: Path, destination: Path, expected: str, *,
                timeout: float = 300, max_bytes: int = 4 * 1024**3) -> dict:
    """Recover an indivisible snapshot to NEW private output, never a live tree.

    Validate every object before creating output, then hash again while copying
    and read back the entire result. A recovery receipt is written only after
    bundle verification and data synchronization have finished. On failure the
    incomplete destination remains; retries must choose a different new path.
    Ownership, ACLs, xattrs and timestamps are intentionally not reinstated.
    Files are 0600 and directories 0700, regardless of recorded original modes.
    """
    platform_check()
    budget = Budget(timeout, max_bytes)
    destination = absolute(destination)
    with ExitStack() as outputs:
        with verified_bundle(bundle, expected, budget) as (entries, payload, source):
            if destination == source or source in destination.parents or destination in source.parents:
                raise SnapshotFailure("source_and_output_overlap")
            output = outputs.enter_context(external_output(destination, bundle, source))
            os.mkdir(b"data", 0o700, dir_fd=output)
            with member(output, b"data", is_directory=True) as data:
                for record in entries[1:]:
                    budget.check()
                    path = bytes.fromhex(record["path_hex"])
                    parent_path, _, name = path.rpartition(b"/")
                    with member(data, parent_path, is_directory=True) as parent:
                        if record["kind"] == "directory":
                            os.mkdir(name, 0o700, dir_fd=parent)
                        else:
                            with member(payload, record["object"].encode()) as saved, new_file(parent, name) as restored:
                                digest = stream(saved, record["size"], budget, restored)
                                if digest != record["sha256"]:
                                    raise SnapshotFailure("payload_changed_during_materialization")
                        os.fsync(parent)
                verify_materialized(data, entries, budget)
                # Synchronize children before their directory entries/parents.
                for record in reversed(entries):
                    if record["kind"] == "directory":
                        with member(data, bytes.fromhex(record["path_hex"]), is_directory=True) as child:
                            os.fsync(child)
                assert_bound(destination / "data", data)
        # Exit verified_bundle BEFORE publishing a success receipt.
        budget.check()
        result = {"schema": "br.runtime-materialization.v1", "status": "materialized",
                  "manifest_sha256": expected, "data_directory": str(destination / "data"),
                  "files": sum(record["kind"] == "file" for record in entries),
                  "bytes": sum(record.get("size", 0) for record in entries),
                  "permissions": "private_0700_0600", "engine_validated": False,
                  "live_installation_performed": False}
        with new_file(output, b"recovery.json") as receipt:
            write_all(receipt, encode_manifest(result))
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    pack = commands.add_parser("capture", help="snapshot one complete, stopped directory")
    pack.add_argument("source", type=Path)
    pack.add_argument("bundle", type=Path)
    pack.add_argument("--writers-stopped", action="store_true")
    check = commands.add_parser("verify", help="verify against the separately retained receipt")
    check.add_argument("bundle", type=Path)
    check.add_argument("--manifest-sha256", required=True)
    restore = commands.add_parser("materialize", help="recover the whole bundle to a NEW external directory")
    restore.add_argument("bundle", type=Path)
    restore.add_argument("destination", type=Path)
    restore.add_argument("--manifest-sha256", required=True)
    for command in (pack, check, restore):
        command.add_argument("--timeout", type=float, default=300)
        command.add_argument("--max-bytes", type=int, default=4 * 1024**3)
    args = parser.parse_args(argv)
    try:
        options = {"timeout": args.timeout, "max_bytes": args.max_bytes}
        if args.command == "capture":
            result = capture(args.source, args.bundle, writers_stopped=args.writers_stopped, **options)
        elif args.command == "materialize":
            result = materialize(args.bundle, args.destination, args.manifest_sha256, **options)
        else:
            result = verify(args.bundle, args.manifest_sha256, **options)
    except (SnapshotFailure, OSError) as error:
        print(json.dumps({"status": "refused_or_incomplete", "reason": str(error),
                          "source_writes_performed": False, "partial_output_retained": True}, ensure_ascii=True))
        return 2
    print(json.dumps(result, indent=2, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
