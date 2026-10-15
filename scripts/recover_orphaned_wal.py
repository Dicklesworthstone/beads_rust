#!/usr/bin/env python3
"""Recover a classic SQLite WAL, preserving every existing source file.

Linux 64-bit only; Python 3.10+ and its stock sqlite3 module are required.
Run --help for the explicit source/destination interface. This is an operator
tool, NOT a br backend or an automatic live checkpoint. No input is deleted,
renamed, chmodded, checkpointed, or opened through a SQLite connection.
By default only a NEW recovery directory is created. Explicit --restore-shm
also installs a verified missing index beside the original, without replacing
any existing file or writing/truncating the main database or WAL.
Explicit --copy-last-commit recovers only into a NEW database when a crashed
transaction left an unfinished tail. It stops at the first invalid frame,
never skips it, and retains the entire original WAL, including that tail.
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
import re
import sqlite3
import stat
import struct
import subprocess
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
NATIVE_PROBE_SCHEMA = "br.orphaned_wal.native_probe.v1"
NATIVE_PROBE_TIMEOUT = 60
NATIVE_WRITE_SCHEMA = "br.orphaned_wal.native_write.v1"


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


def wal_plan(main, wal, max_bytes: int, *, copy_last_commit: bool = False) -> dict:
    """Strict recovery validates EVERY frame. Explicit copy stops at damage.

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
    require(length > 32 and (copy_last_commit or (length - 32) % frame_size == 0),
            "WAL has no frames or has a partial frame")
    latest = {}
    pending = {}
    commits = 0
    final_pages = 0
    last_commit_frame = 0
    validated_frames = 0
    frame_count = (length - 32) // frame_size
    stopped = "partial-frame" if (length - 32) % frame_size else "eof"
    for index in range(frame_count):
        frame = wal.read(frame_size)
        require(len(frame) == frame_size, "WAL changed while validating")
        page, committed_pages, s1, s2, ca, cb = struct.unpack(">6I", frame[:24])
        invalid = None
        if (s1, s2) != (salt1, salt2):
            invalid = ("salt-mismatch", "WAL frame salt mismatch")
        elif not (0 < page < 0xFFFFFFFF and committed_pages < 0xFFFFFFFF):
            invalid = ("invalid-page-number", "invalid WAL page number or commit size")
        if invalid is not None:
            require(copy_last_commit, invalid[1])
            stopped = invalid[0]
            break
        require(page * page_size <= max_bytes and committed_pages * page_size <= max_bytes,
                "WAL page or commit exceeds --max-bytes")
        a, b = checksum(frame[:8], (a, b), big_endian)
        a, b = checksum(frame[24:], (a, b), big_endian)
        if (a, b) != (ca, cb):
            require(copy_last_commit, "invalid WAL frame checksum")
            stopped = "checksum-mismatch"
            break
        validated_frames = index + 1
        # Pages in an unfinished transaction must not shadow their previous
        # committed images, even when every trailing checksum is valid.
        pending[page] = 32 + index * frame_size + 24
        if committed_pages:
            latest.update(pending)
            pending.clear()
            commits += 1
            final_pages = committed_pages
            last_commit_frame = index + 1
    if copy_last_commit:
        require(last_commit_frame > 0, "no committed WAL frame; refusing last-commit copy")
    else:
        require(last_commit_frame == frame_count and final_pages > 0,
                "uncommitted WAL tail; no prefix-only recovery is allowed")
    plan = dict(page_size=page_size, frame_count=last_commit_frame, commits=commits,
                database_pages=final_pages, latest=latest, checksum_big_endian=big_endian)
    if copy_last_commit:
        end = 32 + last_commit_frame * frame_size
        plan["excluded_tail"] = dict(
            offset=end, bytes=length - end,
            physical_complete_frames=frame_count - last_commit_frame,
            validated_uncommitted_frames=validated_frames - last_commit_frame,
            partial_frame_bytes=(length - 32) % frame_size,
            unverified_bytes=length - (32 + validated_frames * frame_size),
            validation_stopped=stopped,
            source_bytes_retained=True,
        )
    return plan


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


def serialized_image_witness(conn, max_bytes: int) -> dict:
    """Hash SQLite's committed pager image, not an application-chosen WAL prefix.

    Only private connections are passed here. Bound allocation before asking
    SQLite to serialize, and do not normalize any header or unused-page bytes.
    The caller owns a read transaction for the complete observation.
    """
    serialize = getattr(conn, "serialize", None)
    require(callable(serialize),
            "page-image verification requires Python 3.11+ with SQLite serialization support")
    pages = conn.execute("PRAGMA main.page_count").fetchone()[0]
    size = conn.execute("PRAGMA main.page_size").fetchone()[0]
    require(type(pages) is int and type(size) is int and pages > 0
            and 512 <= size <= 65536 and size & (size - 1) == 0,
            "invalid private pager dimensions")
    length = pages * size
    require(length <= max_bytes, "private pager image exceeds --max-bytes")
    image = serialize(name="main")
    require(len(image) == length, "SQLite serialized image has an unexpected size")
    return dict(schema="br.orphaned_wal.page_image.v1", bytes=length,
                page_size=size, database_pages=pages, sha256=hashlib.sha256(image).hexdigest())


def verify_page_image(oracle: Path, candidate: Path, raw: dict, plan: dict,
                      max_bytes: int) -> dict | None:
    """Require exact replay equivalence when the independent pager API exists.

    Python 3.10 keeps the existing logical-row verification for ordinary
    tables; no absent API is represented as a successful physical proof.
    Once available, every serialization error or mismatch is a refusal.
    """
    with contextlib.closing(sqlite3.connect(oracle.as_uri() + "?mode=ro", uri=True,
                                          timeout=0)) as conn:
        conn.execute("PRAGMA trusted_schema=OFF")
        conn.execute("PRAGMA query_only=ON")
        conn.execute("BEGIN")
        witness = (serialized_image_witness(conn, max_bytes)
                   if callable(getattr(conn, "serialize", None)) else None)
    # Also enforce this on strict copy and restoration, not just salvage.
    for suffix in ("", "-wal"):
        with open_regular(component(oracle, suffix)) as retained:
            require(digest(retained) == raw[suffix]["sha256"],
                    "page-image oracle changed protected bytes")
    if witness is not None:
        require(witness["database_pages"] == plan["database_pages"]
                and witness["page_size"] == plan["page_size"],
                "WAL plan differs from SQLite's committed pager dimensions")
        with open_regular(candidate) as image:
            require(os.fstat(image.fileno()).st_size == witness["bytes"]
                    and digest(image) == witness["sha256"],
                    "reconstructed page image differs from the WAL-aware SQLite pager")
    return witness


def builtin_virtual_module(sql: str) -> str | None:
    """Recognize only stored FTS5/RTree schemas; do not load arbitrary modules.

    Metadata establishes that this is a virtual table. This conservative SQL
    tokenization only identifies its module, including quoted table names and
    comments; an unfamiliar declaration remains refused.
    """
    tokens = re.findall(r"--[^\n]*(?:\n|$)|/\*[\s\S]*?\*/|\"(?:[^\"]|\"\")*\"|"
                        r"'(?:[^']|'')*'|`(?:[^`]|``)*`|\[[^\]]*\]|[^\s().,]+|[().,]", sql)
    tokens = [token for token in tokens if not token.startswith(("--", "/*"))]
    if [token.upper() for token in tokens[:3]] != ["CREATE", "VIRTUAL", "TABLE"]:
        return None
    position = 3
    if [token.upper() for token in tokens[position:position + 3]] == ["IF", "NOT", "EXISTS"]:
        position += 3
    position += 1  # The engine has already parsed the table identifier.
    if tokens[position:position + 1] == ["."]:
        position += 2
    if len(tokens) <= position + 2 or tokens[position].upper() != "USING":
        return None
    module = tokens[position + 1]
    if len(module) >= 2 and (module[0], module[-1]) in (("'", "'"), ('"', '"'), ("`", "`"), ("[", "]")):
        module = module[1:-1]
    module = module.lower()
    return module if module in ("fts5", "rtree", "rtree_i32") and tokens[position + 2] == "(" else None


def logical_witness(db: Path, *, immutable: bool, max_bytes: int = DEFAULT_MAX_BYTES) -> dict:
    uri = db.as_uri() + ("?immutable=1&mode=ro" if immutable else "?mode=ro")
    with contextlib.closing(sqlite3.connect(uri, uri=True, timeout=0)) as conn:
        conn.execute("PRAGMA trusted_schema=OFF")
        conn.execute("PRAGMA query_only=ON")
        conn.execute("BEGIN")
        integrity = conn.execute("PRAGMA integrity_check").fetchall()
        require(integrity == [("ok",)], f"integrity check failed on private copy: {integrity[:8]}")
        schema = conn.execute("SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema "
                              "ORDER BY type COLLATE BINARY,name COLLATE BINARY").fetchall()
        tables = []
        page_verified = []
        table_kinds = {row[1]: row[2] for row in conn.execute("PRAGMA main.table_list")
                       if row[0] == "main"}
        for kind, name, _, _, sql in schema:
            if kind != "table":
                continue
            if table_kinds.get(name) == "virtual" or (sql and sql.lstrip().upper().startswith("CREATE VIRTUAL TABLE")):
                module = builtin_virtual_module(sql or "")
                require(module is not None,
                        f"virtual table requires engine-specific verification: {name}")
                # Never SELECT virtual hidden columns: FTS5's table-name
                # column is a process-local cursor handle, not stored data.
                # Its real shadow tables are still witnessed below. Every
                # stored page must additionally match the independent pager.
                page_verified.append(dict(name=name, kind="virtual", module=module))
                continue
            ident = quote_identifier(name)
            columns = [r[1] for r in conn.execute(f"PRAGMA table_xinfo({ident})")]
            require(bool(columns), f"cannot inspect columns: {name}")
            aliases = [alias for alias in ("rowid", "_rowid_", "oid")
                       if alias not in {column.lower() for column in columns}]
            if not aliases:
                page_verified.append(dict(name=name, kind="shadowed-rowid"))
            select = [quote_identifier(column) for column in columns]
            if aliases:
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
        witness = dict(schema_sha256=hashlib.sha256(typed_row(
                        [json.dumps(schema, ensure_ascii=True)])).hexdigest(), tables=tables,
                    user_version=conn.execute("PRAGMA user_version").fetchone()[0],
                    application_id=conn.execute("PRAGMA application_id").fetchone()[0])
        if page_verified:
            # These schemas were refused before this image-bound path existed.
            # Missing serialization support cannot fall back to row-only proof.
            witness.update(page_verified_tables=page_verified,
                           page_image=serialized_image_witness(conn, max_bytes))
        return witness


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


def native_row(row) -> bytes:
    """Versioned wire encoding shared with wal_engine_probe.rs, not SQL text.

    Preserve NULL, integers, IEEE float bits, UTF-8 text and arbitrary blobs.
    Length framing and sorting complete encoded rows preserve duplicates while
    making witness equality independent of query plans and row ordering.
    """
    encoded = bytearray(len(row).to_bytes(8, "big"))
    for value in row:
        if value is None:
            encoded += b"n"
        elif type(value) is int:
            encoded += b"i" + struct.pack(">q", value)
        elif type(value) is float:
            encoded += b"r" + struct.pack(">d", value)
        else:
            require(isinstance(value, (str, bytes)), "unsupported native witness value")
            data = value.encode("utf-8") if isinstance(value, str) else value
            encoded += (b"t" if isinstance(value, str) else b"b")
            encoded += len(data).to_bytes(8, "big") + data
    return bytes(encoded)


def native_rows_digest(rows, max_bytes: int) -> dict:
    encoded, total = [], 0
    for row in rows:
        data = native_row(row)
        total += len(data) + 8
        require(total <= max_bytes, "native row witness exceeds --max-bytes")
        encoded.append(data)
    result = hashlib.sha256()
    for data in sorted(encoded):
        result.update(len(data).to_bytes(8, "big"))
        result.update(data)
    return dict(rows=len(encoded), sha256=result.hexdigest())


def native_manifest(oracle: Path, expected: dict, max_bytes: int, *, immutable: bool = False) -> dict:
    """Independent stock-SQLite expectation; no native engine supplied values."""
    uri = oracle.as_uri() + ("?mode=ro&immutable=1" if immutable else "?mode=ro")
    with contextlib.closing(sqlite3.connect(uri, uri=True, timeout=0)) as conn:
        conn.execute("PRAGMA trusted_schema=OFF")
        conn.execute("PRAGMA query_only=ON")
        conn.execute("BEGIN")
        schema = conn.execute("SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema").fetchall()
        tables = []
        for table in expected["tables"]:
            name = table["name"]
            columns = [row[1] for row in conn.execute(f"PRAGMA table_xinfo({quote_identifier(name)})")]
            alias = table["projection"][0]
            alias = alias if alias in ("rowid", "_rowid_", "oid") else None
            projection = ([alias] if alias else []) + [quote_identifier(column) for column in columns]
            witness = native_rows_digest(conn.execute(
                f"SELECT {','.join(projection)} FROM {quote_identifier(name)}"), max_bytes)
            require(witness["rows"] == table["rows"], "native expectation changed row count")
            tables.append(dict(name=name, columns=columns, rowid_alias=alias, **witness))
        skipped = [item["name"] for item in expected.get("page_verified_tables", [])
                   if item["kind"] == "virtual"]
        return dict(schema=NATIVE_PROBE_SCHEMA, schema_rows=native_rows_digest(schema, max_bytes),
                    tables=tables, skipped_virtual=skipped,
                    user_version=conn.execute("PRAGMA user_version").fetchone()[0],
                    application_id=conn.execute("PRAGMA application_id").fetchone()[0])


def verify_native_write(directory: Path, input_db: Path, manifest: dict, manifest_hash: str,
                        expected: dict, max_bytes: int) -> dict:
    """Independently read the native checkpoint image BEFORE installing SHM.

    Native verified:true alone only binds its self-report. Require durable
    write-stage evidence, then use stock SQLite on a NEW main-only copy to
    detect corruption or WAL-only data loss the same engine might not see.
    No source path or receipt-selected path is ever opened through SQLite.
    """
    with contextlib.ExitStack() as stack:
        files, directories = {}, {}

        def hold_directory(path):
            fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
            stack.callback(os.close, fd)
            st = os.fstat(fd)
            require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
                    "native write evidence directory is not owner-controlled")
            directories[path] = (fd, metadata(st))

        def hold_file(path, limit):
            file = stack.enter_context(open_regular(path))
            st = os.fstat(file.fileno())
            require(st.st_uid == os.geteuid() and not st.st_mode & 0o022,
                    "native write evidence file is not owner-controlled")
            require(st.st_size <= limit, "native write evidence exceeds its byte limit")
            witness = dict(**metadata(st), sha256=digest(file))
            files[path] = (file, witness)
            return file, witness

        def document(path):
            file, witness = hold_file(path, RECEIPT_MAX_BYTES)
            file.seek(0)
            def unique(pairs):
                result = {}
                for key, value in pairs:
                    require(key not in result, "duplicate native write receipt key")
                    result[key] = value
                return result
            def nonfinite(value):
                raise RecoveryError(f"non-finite native write receipt value: {value}")
            try:
                value = json.loads(file.read(RECEIPT_MAX_BYTES + 1), object_pairs_hook=unique,
                                   parse_constant=nonfinite)
            except (ValueError, RecursionError) as error:
                raise RecoveryError("invalid native write receipt") from error
            require(isinstance(value, dict), "native write receipt must be an object")
            return value, witness

        receipt, receipt_file = document(directory / "native-write.json")
        require(set(receipt) == {
                    "schema", "verified", "scope", "source_checkpoint_performed", "manifest_sha256",
                    "sandbox", "probe_table", "input_family_unchanged", "committed_write_reopened",
                    "rollback_verified", "checkpoint", "main_only_verified", "main_only_bytes",
                    "main_only_sha256"}
                and receipt["schema"] == NATIVE_WRITE_SCHEMA
                and receipt["scope"] == "fresh-private-copy"
                and receipt["source_checkpoint_performed"] is False
                and receipt["manifest_sha256"] == manifest_hash
                and all(receipt[key] is True for key in ("verified", "input_family_unchanged",
                    "committed_write_reopened", "rollback_verified", "main_only_verified")),
                "native write receipt lacks bound commit/rollback/checkpoint evidence")
        require(same_json(receipt["checkpoint"], [0, 0, 0]), "incomplete native write checkpoint")
        relative = receipt["sandbox"]
        require(isinstance(relative, str) and re.fullmatch(r"native-write-[A-Za-z0-9_-]+", relative),
                "unsafe native write sandbox reference")
        require(receipt["probe_table"] == f"__br_wal_probe_{relative}",
                "native write probe table is not sandbox-bound")
        sandbox = directory / relative
        hold_directory(sandbox)
        hold_directory(sandbox / "main-only")
        require(not os.path.lexists(sandbox / "failed.json"), "native write rehearsal has failure evidence")
        completed, _ = document(sandbox / "complete.json")
        require(same_json(completed, receipt), "native write completion evidence disagrees")
        prepared, _ = document(sandbox / "prepared.json")
        require(same_json(prepared, dict(schema=NATIVE_WRITE_SCHEMA, manifest_sha256=manifest_hash,
                    input_family=str(input_db), sql_write_target=str(sandbox / "beads.db"))),
                "native write preparation does not bind this private family")
        size, sha256 = receipt["main_only_bytes"], receipt["main_only_sha256"]
        require(type(size) is int and 0 < size <= max_bytes
                and isinstance(sha256, str) and re.fullmatch(r"[0-9a-f]{64}", sha256),
                "invalid native write image witness")
        image, image_file = hold_file(sandbox / "main-only" / "beads.db", max_bytes)
        require(image_file["bytes"] == size and image_file["sha256"] == sha256,
                "native write image differs from its receipt")

        # Never use the engine's potentially sidecar-bearing directory as the
        # independent reader target. This fresh directory contains ONLY main.
        checked_dir = directory / "independent-checkpoint"
        checked_dir.mkdir(mode=0o700)
        checked = checked_dir / "beads.db"
        require(copy_new(image, checked, limit=size) == sha256,
                "native write image changed during independent capture")
        with contextlib.closing(sqlite3.connect(checked.as_uri() + "?mode=ro&immutable=1",
                                              uri=True, timeout=0)) as conn:
            conn.execute("PRAGMA trusted_schema=OFF")
            conn.execute("PRAGMA query_only=ON")
            require(conn.execute("PRAGMA integrity_check").fetchall() == [("ok",)],
                    "independent native checkpoint integrity check failed")
        observed = native_manifest(checked, expected, max_bytes, immutable=True)
        require(same_json(observed, manifest),
                "independent native checkpoint lost or changed original schema/rows/metadata")
        with open_regular(checked) as file:
            require(os.fstat(file.fileno()).st_size == size and digest(file) == sha256,
                    "independent checkpoint reader changed main bytes")
        for path, (fd, witness) in directories.items():
            require(metadata(os.fstat(fd)) == witness
                    and metadata(os.stat(path, follow_symlinks=False)) == witness,
                    "native write evidence directory changed during verification")
        for path, (file, witness) in files.items():
            st, named = os.fstat(file.fileno()), os.stat(path, follow_symlinks=False)
            expected_metadata = {key: value for key, value in witness.items() if key != "sha256"}
            require(st.st_nlink == named.st_nlink == 1 and stat.S_ISREG(named.st_mode)
                    and metadata(st) == metadata(named) == expected_metadata
                    and digest(file) == witness["sha256"]
                    and metadata(os.fstat(file.fileno())) == expected_metadata
                    and metadata(os.stat(path, follow_symlinks=False)) == expected_metadata,
                    "native write evidence changed during verification")
        proof = dict(schema="br.orphaned_wal.native_write_check.v1", verified=True,
                     verifier="stock-sqlite", sqlite_version=sqlite3.sqlite_version,
                     manifest_sha256=manifest_hash, native_receipt_sha256=receipt_file["sha256"],
                     main_only_bytes=size, main_only_sha256=sha256)
        write_json(checked_dir / "verified.json", proof)
        sync_directory(checked_dir)
        return proof


def probe_native_index(staging: Path, template_family: Path, oracle: Path, plan: dict, raw: dict,
                       expected: dict, max_bytes: int, verifier: Path) -> dict:
    """Require the actual linked engine to consume the proposed private index.

    Source and namespace exclusion are still held by recover_copy. The child
    only receives a NEW private family; it cannot repair the template because
    the parent's OFD locks prohibit its recovery/checkpoint/write operations.
    No native success, timeout, malformed output or engine failure can skip the
    independent protected-byte and index checks before installation.
    """
    directory = staging / "native-rehearsal"
    directory.mkdir(mode=0o700)
    db = directory / "beads.db"
    for suffix in ("", "-wal", "-shm"):
        with open_regular(component(template_family, suffix)) as source:
            copy_new(source, component(db, suffix), limit=max_bytes)
    manifest = native_manifest(oracle, expected, max_bytes)
    manifest_path = directory / "expected.json"
    write_json(manifest_path, manifest)
    with open_regular(manifest_path) as retained:
        require(os.fstat(retained.fileno()).st_size <= RECEIPT_MAX_BYTES,
                "native manifest exceeds receipt bound")
        manifest_hash = digest(retained)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("BEADS_", "BD_", "BR_", "FSQLITE_"))}
    env.update(HOME=str(directory), USERPROFILE=str(directory),
               XDG_CONFIG_HOME=str(directory / "config"), NO_COLOR="1")
    stdout, stderr = directory / "probe.stdout", directory / "probe.stderr"
    with open_regular(verifier) as binary, open_regular(component(db, "-shm"), writable=True) as pin:
        executable = dict(**metadata(os.fstat(binary.fileno())), sha256=digest(binary))
        pin.seek(0)
        pinned = pin.read()
        ofd_lock(pin, fcntl.F_RDLCK, 128, 1)
        ofd_lock(pin, fcntl.F_WRLCK, 120, 3)
        with stdout.open("xb") as out, stderr.open("xb") as err:
            try:
                result = subprocess.run([str(verifier), "--native-probe", str(manifest_path),
                                         "--db", str(db), "--max-bytes", str(max_bytes)],
                                        stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                        cwd=directory, env=env, timeout=NATIVE_PROBE_TIMEOUT,
                                        check=False)
            except subprocess.TimeoutExpired as error:
                raise RecoveryError("native engine rehearsal timed out; source index not installed") from error
        require(result.returncode == 0,
                f"native engine rehearsal failed ({result.returncode}); inspect {stderr}")
        require(stdout.stat().st_size <= RECEIPT_MAX_BYTES, "oversized native engine receipt")
        def unique(pairs):
            value = {}
            for key, item in pairs:
                require(key not in value, "duplicate native receipt key")
                value[key] = item
            return value
        def nonfinite(value):
            raise RecoveryError(f"non-finite native receipt value: {value}")
        try:
            receipt = json.loads(stdout.read_bytes(), object_pairs_hook=unique,
                                 parse_constant=nonfinite)
        except (ValueError, RecursionError) as error:
            raise RecoveryError("invalid native engine receipt") from error
        require(isinstance(receipt, dict) and set(receipt) == {
                    "schema", "verified", "engine", "manifest_sha256", "engine_version"}
                and receipt.get("schema") == NATIVE_PROBE_SCHEMA
                and receipt.get("verified") is True and receipt.get("engine") == "frankensqlite"
                and receipt.get("manifest_sha256") == manifest_hash
                and isinstance(receipt.get("engine_version"), str)
                and receipt["engine_version"] not in ("", "unknown"),
                "native engine receipt does not attest this manifest")
        pin.seek(0)
        after = pin.read()
        # Normal readers may update only read marks, never headers/page maps.
        require(after[:100] == pinned[:100] and after[120:] == pinned[120:]
                and len(after) == len(pinned), "native engine rewrote the proposed index")
        require(dict(**metadata(os.fstat(binary.fileno())), sha256=digest(binary)) == executable
                and metadata(os.stat(verifier, follow_symlinks=False)) ==
                    {key: value for key, value in executable.items() if key != "sha256"},
                "native verifier executable changed during rehearsal")
    validate_private_index(db, plan, raw)
    with open_regular(manifest_path) as retained:
        require(digest(retained) == manifest_hash, "native expectation changed during rehearsal")
    independent_write = verify_native_write(directory, db, manifest, manifest_hash, expected, max_bytes)
    sync_directory(directory)
    return dict(**receipt, executable_sha256=executable["sha256"],
                protected_private_bytes_unchanged=True, independent_write=independent_write)


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
        require(logical_witness(rehearsal, immutable=False, max_bytes=max_bytes) == expected,
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
                 restore_shm: bool = False, resume_from: Path | None = None,
                 copy_last_commit: bool = False, engine_verifier: Path | None = None) -> dict:
    platform_check()
    require(max_bytes > 0, "--max-bytes must be positive")
    require(not (restore_shm and resume_from is not None),
            "--restore-shm and --resume-from are mutually exclusive")
    require(not (copy_last_commit and (restore_shm or resume_from is not None)),
            "--copy-last-commit is copy-only; it cannot restore or resume a source index")
    require(engine_verifier is None or restore_shm or resume_from is not None,
            "native index rehearsal is only valid with restoration or resume")
    if engine_verifier is not None:
        engine_verifier = engine_verifier.resolve(strict=True)
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
                plan = wal_plan(saved_main, saved_wal, max_bytes,
                                copy_last_commit=copy_last_commit)
                replay(saved_main, saved_wal, plan, staging / "beads.db")
                oracle_dir = staging / "oracle"
                oracle_dir.mkdir(mode=0o700)
                oracle = oracle_dir / "beads.db"
                copy_new(saved_main, oracle, limit=max_bytes)
                copy_new(saved_wal, component(oracle, "-wal"), limit=max_bytes)
            # Independent SQLite WAL reader vs native reconstructed main-only image.
            expected = logical_witness(oracle, immutable=False, max_bytes=max_bytes)
            if copy_last_commit:
                # The independent reader sees the ORIGINAL full WAL, not a
                # prefix manufactured from our decision. It must not repair
                # main/WAL bytes to make the comparison pass.
                for suffix in ("", "-wal"):
                    with open_regular(component(oracle, suffix)) as observed:
                        require(digest(observed) == raw[suffix]["sha256"],
                                "last-commit oracle changed protected bytes")
            actual = logical_witness(staging / "beads.db", immutable=True, max_bytes=max_bytes)
            require(actual == expected, "reconstructed database differs from the WAL-aware oracle")
            page_image = verify_page_image(oracle, staging / "beads.db", raw, plan, max_bytes)
            # Resume rehearses the installed SHM copied from the source, not
            # the freshly regenerated oracle index. Recovery locks on that
            # private copy prevent SQLite from silently repairing the template.
            index = (rehearse_private_index(staging, before if resume else oracle,
                                           plan, raw, expected, max_bytes)
                     if restore_shm or resume else None)
            native_probe = (probe_native_index(staging, before if resume else oracle, oracle,
                                              plan, raw, expected, max_bytes, engine_verifier)
                            if engine_verifier is not None else None)
            attest_source(source, handles, raw)
            for suffix, witness in raw.items():
                if witness is not None:
                    with component(before, suffix).open("rb") as retained:
                        require(digest(retained) == witness["sha256"], "retained evidence changed")
            with (staging / "beads.db").open("rb") as recovered:
                recovered_sha256 = digest(recovered)
                require(page_image is None or (
                    recovered_sha256 == page_image["sha256"]
                    and os.fstat(recovered.fileno()).st_size == page_image["bytes"]),
                    "verified page image changed before publication")
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
            if page_image is not None:
                report["page_image"] = page_image
            if native_probe is not None:
                report["native_probe"] = native_probe
            if copy_last_commit:
                report.update(copy_last_commit_requested=True, checkpoint_performed=False,
                              recovery_scope="last-validated-commit",
                              note="Only the last validated commit is reconstructed; unverified tail "
                                   "bytes may contain later changes that this copy does not recover. All trailing "
                                   "source WAL bytes remain in before/beads.db-wal. "
                                   "This is not authority to restore SHM or replace the source.")
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
                                                        copy_last_commit_requested=copy_last_commit,
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
                        help="maximum bytes per input family component and recovered database (default 1 GiB)")
    parser.add_argument("--engine-verifier", type=Path, help=argparse.SUPPRESS)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--restore-shm", action="store_true",
                        help="also restore an absent source SHM under continuous exclusion; never checkpoint WAL")
    mode.add_argument("--resume-from", type=Path, metavar="PRIOR_EVIDENCE",
                      help="revalidate an interrupted SHM install into NEW evidence; never replace the existing SHM")
    mode.add_argument("--copy-last-commit", action="store_true",
                      help="copy the last validated commit despite an unfinished final transaction; "
                           "stop at the first invalid frame and retain all source WAL bytes")
    args = parser.parse_args()
    try:
        report = recover_copy(args.db, args.output, max_bytes=args.max_bytes,
                              restore_shm=args.restore_shm, resume_from=args.resume_from,
                              copy_last_commit=args.copy_last_commit, engine_verifier=args.engine_verifier)
    except (RecoveryError, OSError) as error:
        print(json.dumps(dict(ok=False, error=str(error))), file=sys.stderr)
        return 1
    print(json.dumps(dict(ok=True, **report), indent=2, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
