#!/usr/bin/env python3
"""Recover a classic SQLite WAL, preserving every existing source file.

Linux 64-bit only; Python 3.10+ and its stock sqlite3 module are required.
Run --help for the explicit source/destination interface. This is an operator
tool, NOT a br backend or an automatic live checkpoint. No input is deleted,
renamed, chmodded, checkpointed, or opened through a SQLite connection.
By default only a NEW recovery directory is created. Explicit --restore-shm
also installs a verified missing index beside the original, without replacing
any existing file or writing/truncating the main database or WAL.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import struct
import sys
import tempfile

try:
    import fcntl
except ImportError:
    fcntl = None

SUFFIXES = ("", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
            "-ns", "-fsqlite-ns-gate", "-fsqlite-ns-use", ".fsqlite-migration-state")
DEFAULT_MAX_BYTES = 1024 * 1024 * 1024
CHUNK = 1024 * 1024
SCHEMA = "br.orphaned_wal.copy.v1"
SHM_SCHEMA = "br.orphaned_wal.missing_index.v1"
SHM_REGION_BYTES = 32768
RESUME_SCHEMA = "br.orphaned_wal.resume.v1"
RECEIPT_MAX_BYTES = 4 * CHUNK


class RecoveryError(Exception):
    """A failed recovery never authorizes publication or alteration of input."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RecoveryError(message)


def component(db: Path, suffix: str) -> Path:
    return db.with_name(db.name + suffix)


def metadata(st: os.stat_result) -> dict:
    return dict(device=st.st_dev, inode=st.st_ino, bytes=st.st_size,
                mode=stat.S_IMODE(st.st_mode), uid=st.st_uid, gid=st.st_gid,
                mtime_ns=st.st_mtime_ns, ctime_ns=st.st_ctime_ns)


def digest(file) -> str:
    file.seek(0)
    result = hashlib.sha256()
    for data in iter(lambda: file.read(CHUNK), b""):
        result.update(data)
    return result.hexdigest()


def open_regular(path: Path, *, writable: bool = False):
    flags = (os.O_RDWR if writable else os.O_RDONLY)
    flags |= os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    fd = os.open(path, flags)
    try:
        st = os.fstat(fd)
        require(stat.S_ISREG(st.st_mode) and st.st_nlink == 1,
                f"refusing non-regular or hard-linked input: {path}")
        require(metadata(os.stat(path, follow_symlinks=False)) == metadata(st),
                f"input name changed identity: {path}")
        return os.fdopen(fd, "r+b" if writable else "rb")
    except BaseException:
        os.close(fd)
        raise


def platform_check() -> None:
    require(sys.platform.startswith("linux") and ctypes.sizeof(ctypes.c_void_p) == 8
            and ctypes.sizeof(ctypes.c_long) == 8 and fcntl is not None
            and hasattr(fcntl, "F_OFD_SETLK"),
            "recovery requires 64-bit Linux OFD locks; no flock fallback is safe")
    require(hasattr(ctypes.CDLL(None), "renameat2"),
            "atomic no-replace directory publication is unavailable")


def ofd_lock(file, kind: int, offset: int, length: int) -> None:
    """Linux 64-bit flock ABI, checked by platform_check before recovery."""
    class Flock(ctypes.Structure):
        _fields_ = [("l_type", ctypes.c_short), ("l_whence", ctypes.c_short),
                    ("l_start", ctypes.c_longlong), ("l_len", ctypes.c_longlong),
                    ("l_pid", ctypes.c_int)]

    lock = Flock(kind, os.SEEK_SET, offset, length, 0)
    fcntl.fcntl(file.fileno(), fcntl.F_OFD_SETLK, bytes(lock))


@contextlib.contextmanager
def source_exclusion(main):
    """Same br authority byte and SQLite main range as db_inode_lock.rs.

    OFD ownership survives unrelated descriptor closes. Both locks remain held
    through source copying, independent verification, and final source attestation.
    These advisory locks are not protection against hostile non-cooperating I/O.
    """
    acquired = []
    try:
        for offset, length in ((2**63 - 2, 1), (0x40000000, 512)):
            try:
                ofd_lock(main, fcntl.F_WRLCK, offset, length)
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EACCES):
                    raise RecoveryError("database busy: stop ALL br and SQLite users; "
                                        "no source files were changed") from error
                raise
            acquired.append((offset, length))
        yield
    finally:
        for offset, length in reversed(acquired):
            ofd_lock(main, fcntl.F_UNLCK, offset, length)


@contextlib.contextmanager
def namespace_exclusion(source: Path, main):
    """Exclude idle FrankenSQLite bindings as well as SQLite main-file users.

    Hold both Linux lock families on existing gate/use sidecars, so neither
    flock nor POSIX/OFD namespace users can overlap the repair. Never create,
    rebind, or remove namespace records. An incomplete pair is not a
    repairable missing-SHM incident. Shared/group-owned repair is not supported.
    The source main-file exclusion must already be held, including when a
    stock-SQLite family has no namespace yet.
    """
    st = os.fstat(main.fileno())
    require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
            "SHM restoration requires an owner-controlled, non-shared database")
    suffixes = ("-fsqlite-ns-gate", "-fsqlite-ns-use")
    paths = [component(source, suffix) for suffix in suffixes]
    present = [os.path.lexists(path) for path in paths]
    require(present[0] == present[1], "incomplete engine namespace; refusing SHM restoration")
    with contextlib.ExitStack() as stack:
        handles = dict.fromkeys(suffixes)
        if present[0]:
            for suffix, path in zip(suffixes, paths):
                file = stack.enter_context(open_regular(path, writable=True))
                st = os.fstat(file.fileno())
                require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
                        "SHM restoration refuses a shared or foreign-owned engine namespace")
                try:
                    fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    ofd_lock(file, fcntl.F_WRLCK, 0, 0)
                except OSError as error:
                    if error.errno in (errno.EAGAIN, errno.EACCES):
                        raise RecoveryError("engine namespace busy: close ALL database users") from error
                    raise
                handles[suffix] = file
        yield handles


def sync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_json(path: Path, value: dict) -> None:
    with path.open("x", encoding="utf-8") as out:
        out.write(json.dumps(value, indent=2, ensure_ascii=True, allow_nan=False) + "\n")
        out.flush()
        os.fsync(out.fileno())


def copy_new(source, destination: Path, *, limit: int = DEFAULT_MAX_BYTES) -> str:
    source.seek(0)
    result = hashlib.sha256()
    count = 0
    with destination.open("xb") as out:
        for data in iter(lambda: source.read(CHUNK), b""):
            count += len(data)
            require(count <= limit, "input grew or exceeded its copy bound")
            out.write(data)
            result.update(data)
        out.flush()
        os.fsync(out.fileno())
    return result.hexdigest()


def check_sidecars(source: Path) -> None:
    # Unknown family members cannot silently disappear from the evidence.
    known = {source.name + suffix for suffix in SUFFIXES}
    unknown = [p.name for p in source.parent.iterdir()
               if p.name.startswith(source.name + "-") and p.name not in known]
    require(not unknown, f"unknown database sidecars require review: {unknown}")


def capture(source: Path, before: Path, main, stack, max_bytes: int, *, locked_handles=None):
    handles = {"": main}
    witnesses = {}
    check_sidecars(source)
    for suffix in SUFFIXES:
        path = component(source, suffix)
        if locked_handles is not None and suffix in locked_handles:
            file = locked_handles[suffix]
            if file is None:
                require(not os.path.lexists(path), f"source component appeared: {path}")
                witnesses[suffix] = None
                continue
            handles[suffix] = file
        elif suffix:
            try:
                handles[suffix] = stack.enter_context(open_regular(path))
            except FileNotFoundError:
                witnesses[suffix] = None
                continue
        file = handles[suffix]
        st = os.fstat(file.fileno())
        require(st.st_size <= max_bytes, f"input exceeds --max-bytes: {path}")
        witness = metadata(st)
        witness["sha256"] = copy_new(file, component(before, suffix), limit=st.st_size)
        witnesses[suffix] = witness
    sync_directory(before.parent)
    attest_source(source, handles, witnesses)
    return handles, witnesses


def attest_source(source: Path, handles: dict, witnesses: dict) -> None:
    check_sidecars(source)
    for suffix, expected in witnesses.items():
        path = component(source, suffix)
        if expected is None:
            require(not os.path.lexists(path), f"source component appeared: {path}")
            continue
        file = handles[suffix]
        observed = metadata(os.stat(path, follow_symlinks=False))
        named = os.stat(path, follow_symlinks=False)
        require(stat.S_ISREG(named.st_mode) and named.st_nlink == 1,
                f"source component is no longer regular: {path}")
        require(observed == metadata(os.fstat(file.fileno())),
                f"source name changed identity: {path}")
        observed["sha256"] = digest(file)
        require(observed == expected, f"source changed during recovery: {path}")
        require(metadata(os.fstat(file.fileno())) == {k: v for k, v in expected.items()
                                                    if k != "sha256"},
                f"source changed while hashing: {path}")
        require(metadata(os.stat(path, follow_symlinks=False)) == metadata(os.fstat(file.fileno())),
                f"source name changed while hashing: {path}")


def checksum(data: bytes, state: tuple[int, int], big_endian: bool) -> tuple[int, int]:
    require(len(data) % 8 == 0, "unaligned WAL checksum input")
    a, b = state
    for x, y in struct.iter_unpack(">II" if big_endian else "<II", data):
        a = (a + x + b) & 0xFFFFFFFF
        b = (b + y + a) & 0xFFFFFFFF
    return a, b


def wal_plan(main, wal, max_bytes: int) -> dict:
    """Validate EVERY frame; no valid-prefix salvage or tail truncation.

    SQLite fileformat2.html sections 4.1/4.2/4.5: last frame per page through
    the final commit defines the reader image; the commit defines its size.
    Checksums cannot establish provenance of a foreign but valid WAL.
    """
    main.seek(0)
    db_header = main.read(100)
    require(len(db_header) == 100 and db_header[:16] == b"SQLite format 3\0",
            "invalid main database header")
    encoded = int.from_bytes(db_header[16:18], "big")
    page_size = 65536 if encoded == 1 else encoded
    require(512 <= page_size <= 65536 and page_size & (page_size - 1) == 0,
            "invalid main database page size")
    require(db_header[18:20] == b"\x02\x02", "source is not a classic WAL-mode database")
    main_bytes = os.fstat(main.fileno()).st_size
    require(main_bytes % page_size == 0, "partial main database page")
    wal.seek(0)
    header = wal.read(32)
    require(len(header) == 32, "partial WAL header")
    magic, version, wal_page_size, _, salt1, salt2, a, b = struct.unpack(">8I", header)
    require(magic in (0x377F0682, 0x377F0683) and version == 3007000,
            "unsupported WAL magic or version")
    require(wal_page_size == page_size, "WAL/main page-size mismatch")
    big_endian = bool(magic & 1)
    require(checksum(header[:24], (0, 0), big_endian) == (a, b),
            "invalid WAL header checksum")
    length = os.fstat(wal.fileno()).st_size
    frame_size = page_size + 24
    require(length > 32 and (length - 32) % frame_size == 0,
            "WAL has no frames or has a partial frame")
    latest = {}
    commits = 0
    final_pages = 0
    frame_count = (length - 32) // frame_size
    for index in range(frame_count):
        frame = wal.read(frame_size)
        require(len(frame) == frame_size, "WAL changed while validating")
        page, committed_pages, s1, s2, ca, cb = struct.unpack(">6I", frame[:24])
        require((s1, s2) == (salt1, salt2), "WAL frame salt mismatch")
        require(0 < page < 0xFFFFFFFF and committed_pages < 0xFFFFFFFF,
                "invalid WAL page number or commit size")
        require(page * page_size <= max_bytes and committed_pages * page_size <= max_bytes,
                "WAL page or commit exceeds --max-bytes")
        a, b = checksum(frame[:8], (a, b), big_endian)
        a, b = checksum(frame[24:], (a, b), big_endian)
        require((a, b) == (ca, cb), "invalid WAL frame checksum")
        latest[page] = 32 + index * frame_size + 24
        if committed_pages:
            commits += 1
        final_pages = committed_pages
    require(final_pages > 0, "uncommitted WAL tail; no prefix-only recovery is allowed")
    return dict(page_size=page_size, frame_count=frame_count, commits=commits,
                database_pages=final_pages, latest=latest, checksum_big_endian=big_endian)


def replay(main, wal, plan: dict, destination: Path) -> None:
    size = plan["page_size"]
    with destination.open("xb") as out:
        for page in range(1, plan["database_pages"] + 1):
            offset = plan["latest"].get(page)
            source = main if offset is None else wal
            source.seek((page - 1) * size if offset is None else offset)
            data = source.read(size)
            require(len(data) == size, f"page {page} absent from both main and committed WAL")
            out.write(data)
        out.flush()
        os.fsync(out.fileno())


def quote_identifier(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def typed_row(row) -> bytes:
    values = []
    for value in row:
        if value is None:
            values.append(["null"])
        elif isinstance(value, bytes):
            values.append(["blob", value.hex()])
        elif isinstance(value, float):
            values.append(["real", value.hex()])
        elif isinstance(value, int):
            values.append(["integer", str(value)])
        else:
            values.append(["text", value])
    return json.dumps(values, ensure_ascii=True, separators=(",", ":")).encode("ascii")


def logical_witness(db: Path, *, immutable: bool) -> dict:
    uri = db.as_uri() + ("?immutable=1&mode=ro" if immutable else "?mode=ro")
    with contextlib.closing(sqlite3.connect(uri, uri=True, timeout=0)) as conn:
        conn.execute("PRAGMA trusted_schema=OFF")
        conn.execute("PRAGMA query_only=ON")
        integrity = conn.execute("PRAGMA integrity_check").fetchall()
        require(integrity == [("ok",)], f"integrity check failed on private copy: {integrity[:8]}")
        schema = conn.execute("SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema "
                              "ORDER BY type COLLATE BINARY,name COLLATE BINARY").fetchall()
        tables = []
        for kind, name, _, _, sql in schema:
            if kind != "table":
                continue
            require(not sql or not sql.lstrip().upper().startswith("CREATE VIRTUAL TABLE"),
                    f"virtual table requires engine-specific verification: {name}")
            ident = quote_identifier(name)
            columns = [r[1] for r in conn.execute(f"PRAGMA table_xinfo({ident})")]
            require(bool(columns), f"cannot inspect columns: {name}")
            aliases = [alias for alias in ("rowid", "_rowid_", "oid")
                       if alias not in {column.lower() for column in columns}]
            require(bool(aliases), f"all hidden-rowid aliases are shadowed: {name}")
            select = [quote_identifier(column) for column in columns]
            try:
                conn.execute(f"SELECT {aliases[0]} FROM {ident} LIMIT 0")
            except sqlite3.OperationalError as error:
                require("no such column" in str(error), f"rowid inspection failed: {error}")
            else:
                select.insert(0, aliases[0])
            query = f"SELECT {','.join(select)} FROM {ident} ORDER BY " + ",".join(
                f"{index + 1} COLLATE BINARY" for index in range(len(select)))
            rows_hash = hashlib.sha256()
            count = 0
            for row in conn.execute(query):
                encoded = typed_row(row)
                rows_hash.update(len(encoded).to_bytes(8, "big"))
                rows_hash.update(encoded)
                count += 1
            tables.append(dict(name=name, projection=select, rows=count, sha256=rows_hash.hexdigest()))
        return dict(schema_sha256=hashlib.sha256(typed_row(
                        [json.dumps(schema, ensure_ascii=True)])).hexdigest(), tables=tables,
                    user_version=conn.execute("PRAGMA user_version").fetchone()[0],
                    application_id=conn.execute("PRAGMA application_id").fetchone()[0])


def validate_private_index(oracle: Path, plan: dict, raw: dict) -> dict:
    """Validate the entire SQLite-generated index against every retained frame.

    See sqlite.org/walformat.html and fsqlite-wal/src/wal_index.rs. SHM scalars
    and hash entries are native endian; salts are raw big-endian WAL bytes.
    A checksummed header alone is NOT evidence of a correct page/hash map.
    """
    for suffix in ("", "-wal"):
        with open_regular(component(oracle, suffix)) as file:
            require(digest(file) == raw[suffix]["sha256"],
                    f"private SQLite reader changed protected component {suffix or 'main'}")
    with open_regular(component(oracle, "-wal")) as wal, \
            open_regular(component(oracle, "-shm")) as shm:
        frame_count = plan["frame_count"]
        require(0 < frame_count < 0xFFFFFFFF, "WAL frame count cannot fit a shared index")
        regions = 1 + (max(0, frame_count - 4062) + 4095) // 4096
        require(os.fstat(shm.fileno()).st_size == regions * SHM_REGION_BYTES,
                "private SHM has an unexpected segment count")
        header = shm.read(136)
        wal_header = wal.read(32)
        native32 = lambda offset: struct.unpack_from("=I", header, offset)[0]
        require(header[:48] == header[48:96] and native32(0) == 3007000
                and native32(4) == 0 and header[12] == 1,
                "private SHM headers are uninitialized or disagree")
        require(checksum(header[:40], (0, 0), sys.byteorder == "big") ==
                struct.unpack_from("=II", header, 40), "private SHM header checksum mismatch")
        encoded_size = 1 if plan["page_size"] == 65536 else plan["page_size"]
        require(header[13] == int(plan["checksum_big_endian"])
                and struct.unpack_from("=H", header, 14)[0] == encoded_size
                and native32(16) == frame_count and native32(20) == plan["database_pages"]
                and header[32:40] == wal_header[16:24], "private SHM does not bind the complete WAL")
        frame_size = plan["page_size"] + 24
        wal.seek(32 + (frame_count - 1) * frame_size + 16)
        require(struct.unpack_from("=II", header, 24) == struct.unpack(">II", wal.read(8)),
                "private SHM final frame checksum mismatch")
        # Never assert that any source pages were checkpointed. SQLite may set
        # nBackfillAttempted to mxFrame during recovery; that is not nBackfill.
        require(native32(96) == 0 and native32(100) == 0
                and native32(128) <= frame_count and native32(132) == 0
                and header[120:128] == b"\0" * 8,
                "private SHM checkpoint or lock state is not a fresh recovery")
        require(all(mark == 0xFFFFFFFF or mark <= frame_count
                    for mark in struct.unpack_from("=5I", header, 100)),
                "private SHM has an invalid reader mark")
        next_frame = 0
        for region in range(regions):
            expected = bytearray(SHM_REGION_BYTES)
            start = 136 if region == 0 else 0
            capacity = 4062 if region == 0 else 4096
            count = min(capacity, frame_count - next_frame)
            for local in range(1, count + 1):
                wal.seek(32 + next_frame * frame_size)
                page = struct.unpack(">I", wal.read(4))[0]
                struct.pack_into("=I", expected, start + 4 * (local - 1), page)
                slot = (page * 383) & 8191
                while struct.unpack_from("=H", expected, 16384 + 2 * slot)[0]:
                    slot = (slot + 1) & 8191
                struct.pack_into("=H", expected, 16384 + 2 * slot, local)
                next_frame += 1
            shm.seek(region * SHM_REGION_BYTES)
            actual = shm.read(SHM_REGION_BYTES)
            require(actual[start:] == expected[start:],
                    f"private SHM page/hash map differs from WAL in segment {region}")
        return dict(schema=SHM_SCHEMA, bytes=regions * SHM_REGION_BYTES,
                    sha256=digest(shm), frame_count=frame_count, segments=regions,
                    byte_order=sys.byteorder, checkpoint_performed=False)


def rehearse_private_index(staging: Path, oracle: Path, plan: dict, raw: dict,
                           expected: dict, max_bytes: int) -> dict:
    index = validate_private_index(oracle, plan, raw)
    rehearsal_dir = staging / "index-rehearsal"
    rehearsal_dir.mkdir(mode=0o700)
    rehearsal = rehearsal_dir / "beads.db"
    for suffix in ("", "-wal", "-shm"):
        with open_regular(component(oracle, suffix)) as file:
            copy_new(file, component(rehearsal, suffix), limit=max(max_bytes, index["bytes"]))
    with open_regular(component(rehearsal, "-shm"), writable=True) as pin:
        # The Unix DMS lock prevents the first SQLite attachment from resetting
        # SHM. Deny write/checkpoint/recovery locks too: the reader must actually
        # consume the transferred index, not silently repair a bad template.
        ofd_lock(pin, fcntl.F_RDLCK, 128, 1)
        ofd_lock(pin, fcntl.F_WRLCK, 120, 3)
        require(logical_witness(rehearsal, immutable=False) == expected,
                "transferred SHM changed the logical database")
        require(digest(pin) == index["sha256"], "transferred SHM was rewritten during rehearsal")
    validate_private_index(rehearsal, plan, raw)
    with open_regular(component(oracle, "-shm")) as template:
        require(copy_new(template, staging / "verified-shm", limit=index["bytes"]) == index["sha256"],
                "private SHM changed before retention")
    sync_directory(rehearsal_dir)
    return index


def restore_missing_index(source: Path, evidence: Path, handles: dict, raw: dict,
                          index: dict) -> dict:
    """Called ONLY with source_exclusion and namespace_exclusion still held.

    All evidence is durable before installing one complete, absent SHM file.
    Failure after the rename must retain the installed cache, never roll back
    over an arriving user or discard any durable database component.
    """
    require(raw["-shm"] is None, "SHM already exists; refusing to replace it")
    scratch = Path(tempfile.mkdtemp(prefix=".br-wal-shm-", dir=source.parent))
    staged_index = scratch / "index"
    with open_regular(evidence / "verified-shm") as template:
        require(copy_new(template, staged_index, limit=index["bytes"]) == index["sha256"],
                "verified SHM template changed before installation")
    with open_regular(staged_index) as retained:
        receipt = dict(**index, database_path=str(source), staging_path=str(scratch),
                       evidence_path=str(evidence), installed_path=str(component(source, "-shm")))
        write_json(evidence / "shm-install-prepared.json", receipt)
        sync_directory(evidence)
        sync_directory(scratch)
        sync_directory(source.parent)
        attest_source(source, handles, raw)
        publish_no_replace(staged_index, component(source, "-shm"))
        sync_directory(source.parent)
        sync_directory(scratch)
        # Preserve all original identities, permissions, timestamps and bytes.
        # Only -shm, which was absent, has a new descriptor/witness.
        installed = metadata(os.fstat(retained.fileno()))
        installed["sha256"] = index["sha256"]
        attest_source(source, {**handles, "-shm": retained}, {**raw, "-shm": installed})
        write_json(evidence / "shm-install-complete.json", receipt)
        sync_directory(evidence)
        return receipt


def publish_no_replace(staging: Path, destination: Path) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    rename = libc.renameat2
    rename.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    # RENAME_NOREPLACE, with both paths on the destination parent's filesystem.
    if rename(-100, os.fsencode(staging), -100, os.fsencode(destination), 1) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(destination))


def same_json(left, right) -> bool:
    """Compare JSON types too: True is not the integer 1 in recovery evidence."""
    options = dict(sort_keys=True, ensure_ascii=True, allow_nan=False, separators=(",", ":"))
    return json.dumps(left, **options) == json.dumps(right, **options)


class ResumeEvidence:
    """Bind old installation evidence; never alter it or trust its path fields.

    The source and engine-namespace exclusions must already be held. Old
    receipts identify a generation, not permission to replace an existing SHM.
    Fresh WAL validation and protected-read rehearsal remain mandatory.
    """

    def __init__(self, source: Path, evidence: Path, raw: dict, stack, max_bytes: int):
        self.path = evidence
        self.stack = stack
        self.files = {}
        self.directories = {}
        self.hold_directory(evidence)
        self.hold_directory(evidence / "before")
        self.prepared = self.document("prepared.json")
        self.verified = self.document("verified.json")
        for document in (self.prepared, self.verified):
            require(document.get("schema") == SCHEMA and document.get("source") == str(source),
                    "restoration evidence does not name this source and schema")
        require(self.verified.get("restore_shm_requested") is True
                and self.verified.get("source_unchanged") is True
                and self.verified.get("live_recovery") is False,
                "resume requires pre-install restoration evidence, not copy-only evidence")
        self.install = self.document("shm-install-prepared.json")
        prior = self.prepared.get("raw")
        require(isinstance(prior, dict) and set(prior) == set(SUFFIXES)
                and prior["-shm"] is None and same_json(self.verified.get("raw"), prior),
                "restoration evidence manifests do not match")
        require(same_json(prior, {**raw, "-shm": None}),
                "source files no longer match the interrupted restoration generation")
        require(raw["-shm"] is not None,
                "installed SHM is missing; use a fresh --restore-shm run, not resume")
        require(raw["-shm"]["uid"] == os.geteuid() and not raw["-shm"]["mode"] & 0o022,
                "resume requires an owner-controlled installed SHM")
        index = self.verified.get("verified_index")
        require(isinstance(index, dict) and index.get("schema") == SHM_SCHEMA
                and index.get("checkpoint_performed") is False,
                "restoration evidence lacks a verified index")
        require(same_json(index.get("bytes"), raw["-shm"]["bytes"])
                and index.get("sha256") == raw["-shm"]["sha256"],
                "installed SHM does not match the retained verified index")
        require(set(self.install) == set(index) | {
                    "database_path", "staging_path", "evidence_path", "installed_path"}
                and same_json({key: self.install.get(key) for key in index}, index)
                and self.install.get("database_path") == str(source)
                and self.install.get("installed_path") == str(component(source, "-shm"))
                and self.install.get("evidence_path") == str(evidence)
                and isinstance(self.install.get("staging_path"), str),
                "installation evidence does not match this source and verified index")
        # Only fixed paths beneath the explicitly selected evidence directory
        # are opened. No path supplied by a receipt is followed.
        for suffix, witness in prior.items():
            archived = component(evidence / "before" / "beads.db", suffix)
            if witness is None:
                require(not os.path.lexists(archived), "unexpected prior evidence component")
            else:
                self.payload(archived, witness["bytes"], witness["sha256"], max_bytes)
        self.payload(evidence / "verified-shm", index["bytes"], index["sha256"], max_bytes)
        require(self.verified.get("database_path") == str(evidence / "beads.db"),
                "verified evidence names an unexpected recovered copy")
        image = self.hold_file(evidence / "beads.db", max_bytes)
        require(image["sha256"] == self.verified.get("recovered_sha256"),
                "recovered evidence image no longer matches its receipt")
        self.attest()

    def hold_directory(self, path):
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        self.stack.callback(os.close, fd)
        st = os.fstat(fd)
        require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
                "resume requires owner-controlled evidence directories")
        self.directories[path] = (fd, metadata(st))

    def hold_file(self, path, limit):
        file = self.stack.enter_context(open_regular(path))
        st = os.fstat(file.fileno())
        require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
                "resume requires owner-controlled evidence files")
        require(st.st_size <= limit, "recovery evidence exceeds its size bound")
        witness = dict(**metadata(st), sha256=digest(file))
        self.files[path] = (file, witness)
        return witness

    def document(self, name):
        path = self.path / name
        self.hold_file(path, RECEIPT_MAX_BYTES)
        file = self.files[path][0]
        file.seek(0)

        def unique(pairs):
            result = {}
            for key, value in pairs:
                require(key not in result, f"duplicate recovery receipt key: {key}")
                result[key] = value
            return result

        def nonfinite(value):
            raise RecoveryError(f"non-finite recovery receipt value: {value}")

        try:
            value = json.loads(file.read(RECEIPT_MAX_BYTES + 1), object_pairs_hook=unique,
                               parse_constant=nonfinite)
        except (ValueError, RecursionError) as error:
            raise RecoveryError(f"invalid recovery receipt {name}: {error}") from error
        require(isinstance(value, dict), "recovery receipt must be an object")
        return value

    def payload(self, path, size, sha256, limit):
        witness = self.hold_file(path, limit)
        require(same_json(witness["bytes"], size) and witness["sha256"] == sha256,
                f"retained evidence payload does not match: {path}")

    def verify_result(self, plan, index, logical, image_sha256):
        require(same_json(index, self.verified["verified_index"])
                and same_json(logical, self.verified.get("logical"))
                and same_json({key: value for key, value in plan.items() if key != "latest"},
                              self.verified.get("wal"))
                and image_sha256 == self.verified.get("recovered_sha256"),
                "fresh recovery does not match the interrupted restoration evidence")

    def attest(self):
        for path, (fd, expected) in self.directories.items():
            require(metadata(os.fstat(fd)) == expected
                    and metadata(os.stat(path, follow_symlinks=False)) == expected,
                    "recovery evidence directory changed identity or metadata")
        for path, (file, expected) in self.files.items():
            observed = dict(**metadata(os.fstat(file.fileno())), sha256=digest(file))
            require(observed == expected
                    and metadata(os.fstat(file.fileno())) == {key: value for key, value
                        in expected.items() if key != "sha256"}
                    and metadata(os.stat(path, follow_symlinks=False)) == metadata(os.fstat(file.fileno())),
                    "recovery evidence changed during revalidation")

    def receipt(self):
        return dict(schema=RESUME_SCHEMA, evidence_path=str(self.path),
                    existing_index_revalidated=True, prior_evidence_unchanged=True,
                    receipts_sha256={name: self.files[self.path / name][1]["sha256"]
                        for name in ("prepared.json", "verified.json", "shm-install-prepared.json")})


def recover_copy(source: Path, destination: Path, *, max_bytes: int = DEFAULT_MAX_BYTES,
                 restore_shm: bool = False, resume_from: Path | None = None) -> dict:
    platform_check()
    require(max_bytes > 0, "--max-bytes must be positive")
    require(not (restore_shm and resume_from is not None),
            "--restore-shm and --resume-from are mutually exclusive")
    source = source.parent.resolve(strict=True) / source.name
    destination = destination.parent.resolve(strict=True) / destination.name
    if resume_from is not None:
        resume_from = resume_from.parent.resolve(strict=True) / resume_from.name
        require(not destination.is_relative_to(resume_from),
                "new output must be outside prior recovery evidence")
    require(not os.path.lexists(destination), "destination already exists; nothing will be overwritten")
    staging = None
    old_umask = os.umask(0o077)
    try:
        with contextlib.ExitStack() as stack:
            main = stack.enter_context(open_regular(source, writable=True))
            stack.enter_context(source_exclusion(main))
            namespace = None
            if restore_shm or resume_from is not None:
                namespace = stack.enter_context(namespace_exclusion(source, main))
            if restore_shm:
                require(not os.path.lexists(component(source, "-shm")),
                        "SHM already exists; refusing to replace it")
            staging = Path(tempfile.mkdtemp(prefix=".br-wal-copy-", dir=destination.parent))
            before_dir = staging / "before"
            before_dir.mkdir(mode=0o700)
            before = before_dir / "beads.db"
            handles, raw = capture(source, before, main, stack, max_bytes, locked_handles=namespace)
            resume = (ResumeEvidence(source, resume_from, raw, stack, max_bytes)
                      if resume_from is not None else None)
            write_json(staging / "prepared.json", dict(schema=SCHEMA, source=str(source), raw=raw))
            require(raw["-wal"] is not None, "source WAL is missing")
            for suffix in ("-journal", ".fsqlite-migration-state"):
                require(raw[suffix] is None or raw[suffix]["bytes"] == 0,
                        f"nonempty {suffix} requires engine-specific recovery")
            with before.open("rb") as saved_main, component(before, "-wal").open("rb") as saved_wal:
                plan = wal_plan(saved_main, saved_wal, max_bytes)
                replay(saved_main, saved_wal, plan, staging / "beads.db")
                oracle_dir = staging / "oracle"
                oracle_dir.mkdir(mode=0o700)
                oracle = oracle_dir / "beads.db"
                copy_new(saved_main, oracle, limit=max_bytes)
                copy_new(saved_wal, component(oracle, "-wal"), limit=max_bytes)
            # Independent SQLite WAL reader vs native reconstructed main-only image.
            expected = logical_witness(oracle, immutable=False)
            actual = logical_witness(staging / "beads.db", immutable=True)
            require(actual == expected, "reconstructed database differs from the WAL-aware oracle")
            # Resume rehearses the installed SHM copied from the source, not
            # the freshly regenerated oracle index. Recovery locks on that
            # private copy prevent SQLite from silently repairing the template.
            index = (rehearse_private_index(staging, before if resume else oracle,
                                           plan, raw, expected, max_bytes)
                     if restore_shm or resume else None)
            attest_source(source, handles, raw)
            for suffix, witness in raw.items():
                if witness is not None:
                    with component(before, suffix).open("rb") as retained:
                        require(digest(retained) == witness["sha256"], "retained evidence changed")
            with (staging / "beads.db").open("rb") as recovered:
                recovered_sha256 = digest(recovered)
            if resume:
                resume.verify_result(plan, index, actual, recovered_sha256)
                # A crash may have occurred before the original directory
                # sync. Flush this exact retained index and its name, without
                # changing a byte or taking the old no-replace install path.
                os.fsync(handles["-shm"].fileno())
                sync_directory(source.parent)
                resume.attest()
            report = dict(schema=SCHEMA, source=str(source), source_unchanged=True,
                          live_recovery=False, database_path=str(destination / "beads.db"),
                          sqlite_version=sqlite3.sqlite_version, recovered_sha256=recovered_sha256,
                          wal={k: v for k, v in plan.items() if k != "latest"}, logical=actual,
                          raw=raw, scope="classic SQLite WAL; engine certificates retained, not reused")
            if restore_shm:
                report["restore_shm_requested"] = True
                report["verified_index"] = index
            if resume:
                report.update(resume=resume.receipt(), verified_index=index,
                              checkpoint_performed=False, database_path=str(source),
                              recovered_copy_path=str(destination / "beads.db"))
            write_json(staging / "verified.json", report)
            sync_directory(oracle_dir)
            sync_directory(staging)
            # Source names/hashes must still match immediately before publication.
            attest_source(source, handles, raw)
            if resume:
                resume.attest()
            publish_no_replace(staging, destination)
            staging = destination
            sync_directory(destination.parent)
            if restore_shm:
                report["restored_shm"] = restore_missing_index(source, destination, handles, raw, index)
                report.update(source_unchanged=False, existing_source_files_unchanged=True,
                              live_recovery=True, database_path=str(source),
                              recovered_copy_path=str(destination / "beads.db"))
            if resume:
                attest_source(source, handles, raw)
                resume.attest()
            write_json(destination / "complete.json", report)
            sync_directory(destination)
            return report
    except (OSError, sqlite3.Error, RecoveryError, ValueError) as error:
        if staging is not None:
            try:
                write_json(staging / "failed.json", dict(schema=SCHEMA, error=str(error),
                                                        source=str(source), live_recovery=restore_shm,
                                                        resume_from=str(resume_from) if resume_from else None,
                                                        restore_shm_requested=restore_shm))
                sync_directory(staging)
            except OSError:
                pass
        raise RecoveryError(f"{error}; evidence retained at {staging}" if staging else str(error)) from error
    finally:
        os.umask(old_umask)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, type=Path, help="original database; existing files never modified")
    parser.add_argument("--output", required=True, type=Path, help="NEW recovery directory; must not exist")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES,
                        help="maximum bytes per input component and recovered database (default 1 GiB)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--restore-shm", action="store_true",
                        help="also restore an absent source SHM under continuous exclusion; never checkpoint WAL")
    mode.add_argument("--resume-from", type=Path, metavar="PRIOR_EVIDENCE",
                      help="revalidate an interrupted SHM install into NEW evidence; never replace the existing SHM")
    args = parser.parse_args()
    try:
        report = recover_copy(args.db, args.output, max_bytes=args.max_bytes,
                              restore_shm=args.restore_shm, resume_from=args.resume_from)
    except (RecoveryError, OSError) as error:
        print(json.dumps(dict(ok=False, error=str(error))), file=sys.stderr)
        return 1
    print(json.dumps(dict(ok=True, **report), indent=2, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
