#!/usr/bin/env python3
"""Execute the copy-recovery implementation against real disposable SQLite WALs.

This tests scripts/recover_orphaned_wal.py, not a br binary. All fixtures and
failure evidence are retained. No existing/user-supplied database is accepted.
Optional actual-br qualification: python3 ... --br /absolute/path/to/br
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import selectors
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "recover_orphaned_wal.py"
SPEC = importlib.util.spec_from_file_location("wal_copy", SCRIPT)
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)

STAMP = "2026-10-13T00:00:00Z"
REASON = "committed only in WAL; must survive"


def ready_line(child: subprocess.Popen) -> str:
    with selectors.DefaultSelector() as selector:
        selector.register(child.stdout, selectors.EVENT_READ)
        recovery.require(bool(selector.select(timeout=10)), 'fixture process did not become ready')
        return child.stdout.readline().strip()


def family(db: Path) -> dict:
    return {suffix: recovery.component(db, suffix).read_bytes()
            if recovery.component(db, suffix).exists() else None
            for suffix in recovery.SUFFIXES}


def fixture(root: Path, *, page_size: int = 4096, shrink: bool = False) -> Path:
    original = root / "writer.db"
    script = r'''
import os, sqlite3, sys
db, page_size, shrink, stamp, reason = sys.argv[1:]
conn = sqlite3.connect(db)
conn.execute('PRAGMA page_size=' + page_size)
conn.execute('PRAGMA auto_vacuum=FULL')
conn.execute('CREATE TABLE issues (id TEXT PRIMARY KEY, status TEXT, closed_at TEXT, close_reason TEXT)')
conn.execute("INSERT INTO issues VALUES ('case-1', 'open', NULL, '')")
conn.execute('CREATE TABLE extra (id INTEGER PRIMARY KEY AUTOINCREMENT, n, payload BLOB, note TEXT)')
conn.execute('CREATE INDEX extra_note ON extra(note COLLATE NOCASE)')
conn.executemany('INSERT INTO extra(n,payload,note) VALUES (?,?,?)',
                 [(i, b'\x00\xff' * 600, "text ' λ " + str(i)) for i in range(80)])
conn.execute('CREATE TABLE keyed (k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID')
conn.execute("INSERT INTO keyed VALUES ('key', x'0001ff')")
conn.execute('CREATE TABLE generated (a INT, b INT GENERATED ALWAYS AS (a*2) STORED)')
conn.execute('INSERT INTO generated(a) VALUES (4)')
conn.execute('CREATE TABLE audit (event TEXT)')
conn.execute("CREATE TRIGGER issue_audit AFTER UPDATE ON issues BEGIN INSERT INTO audit VALUES ('closed'); END")
conn.execute('PRAGMA user_version=123')
conn.execute('PRAGMA application_id=456')
conn.commit()
assert conn.execute('PRAGMA journal_mode=WAL').fetchone()[0] == 'wal'
conn.execute('PRAGMA wal_autocheckpoint=0')
assert conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0,0,0)
conn.execute("UPDATE issues SET status='closed', closed_at=?, close_reason=? WHERE id='case-1'", (stamp,reason))
conn.execute("INSERT INTO extra(n,payload,note) VALUES (?, ?, ?)", (3.125,b'wal blob', 'WAL only'))
conn.commit()
conn.execute("UPDATE extra SET n='changed', payload=x'beef' WHERE id=1")
conn.commit()
if shrink == 'True':
    conn.execute('DELETE FROM extra WHERE id>2')
    conn.commit()
os._exit(0)
'''
    subprocess.run([sys.executable, "-I", "-c", script, str(original), str(page_size),
                    str(shrink), STAMP, REASON], check=True, timeout=20)
    source_dir = root / "source"
    source_dir.mkdir()
    source = source_dir / "-selected ' $HOME ; λ.db"
    # Omit SHM in a NEW copy; never remove a sidecar from an open family.
    for suffix, data in family(original).items():
        if data is not None and suffix != "-shm":
            with recovery.component(source, suffix).open("xb") as out:
                out.write(data)
    return source


def rows(db: Path, query: str, *, immutable: bool = True):
    uri = db.as_uri() + ("?mode=ro&immutable=1" if immutable else "?mode=ro")
    with contextlib.closing(sqlite3.connect(uri, uri=True)) as conn:
        return conn.execute(query).fetchall()


def rewrite_checksums(wal: bytearray, *, big_endian: bool = False) -> None:
    wal[:4] = (0x377F0683 if big_endian else 0x377F0682).to_bytes(4, "big")
    state = recovery.checksum(wal[:24], (0, 0), big_endian)
    wal[24:32] = struct.pack(">II", *state)
    size = int.from_bytes(wal[8:12], 'big') + 24
    for start in range(32, len(wal), size):
        state = recovery.checksum(wal[start:start + 8], state, big_endian)
        state = recovery.checksum(wal[start + 24:start + size], state, big_endian)
        wal[start + 16:start + 24] = struct.pack(">II", *state)


@unittest.skipUnless(sys.platform.startswith("linux") and ctypes.sizeof(ctypes.c_void_p) == 8,
                     "qualified recovery requires 64-bit Linux")
class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-wal-copy-test-"))
        self.db = fixture(self.root)
        self.output = self.root / "recovered"

    def assert_refused(self, pattern: str, **kwargs):
        before = family(self.db)
        with self.assertRaisesRegex(recovery.RecoveryError, pattern):
            recovery.recover_copy(self.db, self.output, **kwargs)
        self.assertEqual(before, family(self.db))
        self.assertFalse(self.output.exists())

    def test_wal_only_changes_whole_database_and_hidden_rowids_survive(self):
        before = family(self.db)
        self.assertEqual(rows(self.db, 'SELECT status FROM issues'), [('open',)])
        report = recovery.recover_copy(self.db, self.output)
        recovered = self.output / "beads.db"
        self.assertEqual(rows(recovered, 'SELECT status,closed_at,close_reason FROM issues'),
                         [('closed', STAMP, REASON)])
        self.assertEqual(rows(recovered, 'SELECT n,payload FROM extra WHERE id=1'), [('changed', b'\xbe\xef')])
        self.assertEqual(rows(recovered, 'SELECT COUNT(*) FROM extra'), [(81,)])
        self.assertEqual(rows(recovered, 'SELECT * FROM keyed'), [('key', b'\x00\x01\xff')])
        self.assertEqual(rows(recovered, 'SELECT * FROM generated'), [(4, 8)])
        self.assertEqual(rows(recovered, 'SELECT * FROM audit'), [('closed',)])
        self.assertEqual(rows(recovered, 'PRAGMA integrity_check'), [('ok',)])
        self.assertEqual(rows(recovered, 'PRAGMA user_version'), [(123,)])
        self.assertEqual(rows(recovered, 'PRAGMA application_id'), [(456,)])
        self.assertEqual(before, family(self.db))
        self.assertFalse(recovery.component(recovered, '-wal').exists())
        self.assertEqual(report, json.loads((self.output / 'complete.json').read_text()))
        self.assertEqual(report['wal']['commits'], 2)
        self.assertIn('rowid', next(t for t in report['logical']['tables'] if t['name'] == 'extra')['projection'])
        for suffix, data in before.items():
            if data is not None:
                self.assertEqual(recovery.component(self.output / 'before' / 'beads.db', suffix).read_bytes(), data)
        # A real post-recovery transaction is possible; only the new copy is written.
        with contextlib.closing(sqlite3.connect(recovered)) as conn:
            conn.execute("INSERT INTO audit VALUES ('post-recovery write')")
            conn.commit()
        self.assertEqual(rows(recovered, 'SELECT COUNT(*) FROM audit'), [(2,)])
        self.assertEqual(before, family(self.db))

    def test_page_size_boundaries_and_committed_shrink(self):
        for size in (512, 65536):
            with self.subTest(page_size=size):
                root = self.root / str(size)
                root.mkdir()
                source = fixture(root, page_size=size, shrink=True)
                before = family(source)
                output = root / 'recovered'
                report = recovery.recover_copy(source, output)
                self.assertEqual(rows(output / 'beads.db', 'SELECT COUNT(*) FROM extra'), [(2,)])
                self.assertEqual(rows(output / 'beads.db', 'PRAGMA page_size'), [(size,)])
                self.assertEqual((output / 'beads.db').stat().st_size, size * report['wal']['database_pages'])
                self.assertLess((output / 'beads.db').stat().st_size, len(before['']))
                self.assertEqual(before, family(source))

    def test_big_endian_wal_checksums_match_stock_reader(self):
        wal_path = recovery.component(self.db, '-wal')
        data = bytearray(wal_path.read_bytes())
        rewrite_checksums(data, big_endian=True)
        wal_path.write_bytes(data)
        report = recovery.recover_copy(self.db, self.output)
        self.assertTrue(report['wal']['checksum_big_endian'])
        self.assertEqual(rows(self.output / 'beads.db', 'SELECT status FROM issues'), [('closed',)])

    def test_partial_frame_rejected_without_source_mutation(self):
        wal = recovery.component(self.db, '-wal')
        wal.write_bytes(wal.read_bytes()[:-1])
        self.assert_refused('partial frame')

    def test_header_and_frame_damage_rejected(self):
        wal = recovery.component(self.db, '-wal')
        original = wal.read_bytes()
        for index, pattern in ((0, 'magic'), (24, 'header checksum'),
                               (40, 'salt mismatch'), (56, 'frame checksum')):
            with self.subTest(index=index):
                data = bytearray(original)
                data[index] ^= 1
                wal.write_bytes(data)
                self.assert_refused(pattern)

    def test_complete_but_uncommitted_tail_rejected(self):
        wal = recovery.component(self.db, '-wal')
        data = bytearray(wal.read_bytes())
        size = int.from_bytes(data[8:12], 'big') + 24
        data[len(data) - size + 4:len(data) - size + 8] = b'\0' * 4
        rewrite_checksums(data)
        wal.write_bytes(data)
        self.assert_refused('uncommitted WAL tail')

    def test_nonempty_rollback_journal_is_not_ignored(self):
        recovery.component(self.db, '-journal').write_bytes(b'pending rollback')
        self.assert_refused('engine-specific recovery')

    def test_migration_bookkeeping_is_not_ignored(self):
        recovery.component(self.db, '.fsqlite-migration-state').write_bytes(b'pending')
        self.assert_refused('engine-specific recovery')

    def test_unknown_sidecar_is_not_silently_discarded(self):
        recovery.component(self.db, '-future-format').write_bytes(b'unknown')
        self.assert_refused('unknown database sidecars')

    def test_certificates_and_namespace_bytes_retained_not_reused(self):
        for suffix in ('-wal-cert', '-wal-cert-head', '-fsqlite-ns-gate', '-fsqlite-ns-use'):
            recovery.component(self.db, suffix).write_bytes(b'engine evidence' + suffix.encode())
        before = family(self.db)
        recovery.recover_copy(self.db, self.output)
        for suffix in ('-wal-cert', '-wal-cert-head', '-fsqlite-ns-gate', '-fsqlite-ns-use'):
            self.assertEqual(recovery.component(self.output / 'before' / 'beads.db', suffix).read_bytes(), before[suffix])
            self.assertFalse(recovery.component(self.output / 'beads.db', suffix).exists())
        self.assertEqual(before, family(self.db))

    def test_hardlinked_source_refused(self):
        os.link(self.db, self.root / 'alias.db')
        self.assert_refused('hard-linked')

    def test_symlinked_source_refused(self):
        alias = self.root / 'alias.db'
        alias.symlink_to(self.db)
        with self.assertRaisesRegex(recovery.RecoveryError, 'Too many levels|symbolic link'):
            recovery.recover_copy(alias, self.output)
        self.assertFalse(self.output.exists())

    def test_nonregular_sidecar_refused(self):
        os.mkfifo(recovery.component(self.db, '-shm'))
        # Do not use family(): opening a FIFO there would block the test.
        with self.assertRaisesRegex(recovery.RecoveryError, 'non-regular'):
            recovery.recover_copy(self.db, self.output)
        self.assertFalse(self.output.exists())

    def test_existing_destination_is_never_overwritten(self):
        self.output.mkdir()
        sentinel = self.output / 'sentinel'
        sentinel.write_bytes(b'keep me')
        before = family(self.db)
        with self.assertRaisesRegex(recovery.RecoveryError, 'already exists'):
            recovery.recover_copy(self.db, self.output)
        self.assertEqual(sentinel.read_bytes(), b'keep me')
        self.assertEqual(before, family(self.db))

    def test_destination_race_is_atomic_no_replace(self):
        publish = recovery.publish_no_replace
        def race(staging, destination):
            destination.mkdir()
            (destination / 'sentinel').write_bytes(b'racing entry')
            publish(staging, destination)
        with mock.patch.object(recovery, 'publish_no_replace', side_effect=race):
            with self.assertRaisesRegex(recovery.RecoveryError, 'File exists'):
                recovery.recover_copy(self.db, self.output)
        self.assertEqual((self.output / 'sentinel').read_bytes(), b'racing entry')
        self.assertFalse((self.output / 'beads.db').exists())
        self.assertTrue(any(self.root.glob('.br-wal-copy-*/failed.json')))

    def test_injected_failure_keeps_source_and_partial_evidence(self):
        with mock.patch.object(recovery, 'replay', side_effect=OSError('injected disk failure')):
            self.assert_refused('injected disk failure')
        self.assertTrue(any(self.root.glob('.br-wal-copy-*/before/beads.db-wal')))
        self.assertTrue(any(self.root.glob('.br-wal-copy-*/failed.json')))

    def test_integrity_success_without_the_commit_is_rejected(self):
        def discard_wal(main, wal, plan, destination):
            recovery.copy_new(main, destination)
        with mock.patch.object(recovery, 'replay', side_effect=discard_wal):
            self.assert_refused('differs from the WAL-aware oracle')

    def test_source_change_before_publication_refused(self):
        witness = recovery.logical_witness
        def changing(db, *, immutable):
            result = witness(db, immutable=immutable)
            if immutable:
                recovery.component(self.db, '-shm').write_bytes(b'changed by noncooperating I/O')
            return result
        with mock.patch.object(recovery, 'logical_witness', side_effect=changing):
            with self.assertRaisesRegex(recovery.RecoveryError, 'source component appeared'):
                recovery.recover_copy(self.db, self.output)
        self.assertFalse(self.output.exists())

    def test_max_bytes_refuses_oversized_input(self):
        self.assert_refused('exceeds --max-bytes', max_bytes=512)

    def test_forged_commit_size_refused_before_output_allocation(self):
        wal = recovery.component(self.db, '-wal')
        data = bytearray(wal.read_bytes())
        size = int.from_bytes(data[8:12], 'big') + 24
        data[len(data) - size + 4:len(data) - size + 8] = (0xFFFFFFFE).to_bytes(4, 'big')
        rewrite_checksums(data)
        wal.write_bytes(data)
        self.assert_refused('commit exceeds --max-bytes')
        self.assertFalse(any(self.root.glob('.br-wal-copy-*/beads.db')))

    def test_recovered_files_are_private_source_modes_unchanged(self):
        self.db.chmod(0o640)
        recovery.component(self.db, '-wal').chmod(0o640)
        recovery.recover_copy(self.db, self.output)
        self.assertEqual(self.db.stat().st_mode & 0o777, 0o640)
        self.assertEqual(recovery.component(self.db, '-wal').stat().st_mode & 0o777, 0o640)
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o700)
        for path in self.output.rglob('*'):
            if path.is_file():
                self.assertEqual(path.stat().st_mode & 0o077, 0, str(path))

    def test_existing_stock_reader_excludes_recovery(self):
        script = """
import os, sqlite3, sys
conn = sqlite3.connect(sys.argv[1], timeout=0)
conn.execute('BEGIN')
conn.execute('SELECT * FROM issues').fetchall()
print('pinned', flush=True)
sys.stdin.readline()
os._exit(0)
"""
        with subprocess.Popen([sys.executable, '-I', '-u', '-c', script, str(self.db)],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
            try:
                self.assertEqual(ready_line(child), 'pinned')
                self.assert_refused('database busy')
                self.assertFalse(any(self.root.glob('.br-wal-copy-*')))
            finally:
                child.stdin.write('exit\n')
                child.stdin.flush()
                child.wait(timeout=10)
        # After peer exit, the source is recoverable without weakening the lock.
        recovery.recover_copy(self.db, self.output)

    def test_arriving_writer_is_blocked_during_copy_and_attestation(self):
        attest = recovery.attest_source
        def probe(*args):
            # An unrelated open/close must not release the held OFD locks.
            self.db.read_bytes()
            script = """
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1], timeout=0)
try:
    conn.execute("UPDATE issues SET status='lost'")
    conn.commit()
except sqlite3.OperationalError as error:
    assert error.sqlite_errorcode == sqlite3.SQLITE_BUSY
    sys.exit(42)
sys.exit(99)
"""
            child = subprocess.run([sys.executable, '-I', '-c', script, str(self.db)],
                                   capture_output=True, text=True, timeout=10, check=False)
            self.assertEqual(child.returncode, 42, child.stderr)
            attest(*args)
        with mock.patch.object(recovery, 'attest_source', side_effect=probe):
            recovery.recover_copy(self.db, self.output)

    def test_cli_uses_argv_paths_not_shell_interpolation(self):
        output = self.root / "output ' $HOME ; λ"
        result = subprocess.run([sys.executable, str(SCRIPT), '--db', str(self.db), '--output', str(output)],
                                capture_output=True, text=True, timeout=20, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report['ok'])
        self.assertEqual(Path(report['database_path']), output / 'beads.db')
        self.assertFalse(report['live_recovery'])


    def test_process_death_retains_source_and_never_marks_partial_output_complete(self):
        script = r'''
import importlib.util, os, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('recovery', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
stage = sys.argv[4]
if stage == 'replay':
    def crash(main, wal, plan, destination):
        with destination.open('xb') as out:
            out.write(b'partial candidate')
            out.flush()
            os.fsync(out.fileno())
        os._exit(73)
    module.replay = crash
elif stage == 'verify':
    witness = module.logical_witness
    def crash(db, *, immutable):
        result = witness(db, immutable=immutable)
        if immutable:
            os._exit(73)
        return result
    module.logical_witness = crash
else:
    publish = module.publish_no_replace
    def crash(staging, destination):
        publish(staging, destination)
        os._exit(73)
    module.publish_no_replace = crash
module.recover_copy(Path(sys.argv[2]), Path(sys.argv[3]))
raise AssertionError('crash injection was not reached')
'''
        before = family(self.db)
        for stage in ('replay', 'verify', 'publish'):
            with self.subTest(stage=stage):
                parent = self.root / stage
                parent.mkdir()
                output = parent / 'result'
                result = subprocess.run([sys.executable, '-I', '-c', script, str(SCRIPT),
                                         str(self.db), str(output), stage], capture_output=True,
                                        text=True, timeout=20, check=False)
                self.assertEqual(result.returncode, 73, result.stderr)
                self.assertEqual(before, family(self.db))
                retained = output if output.exists() else next(parent.glob('.br-wal-copy-*'))
                self.assertFalse((retained / 'complete.json').exists())
                self.assertEqual((retained / 'before' / 'beads.db-wal').read_bytes(), before['-wal'])
                # Kernel releases the dead process's locks; a fresh copy can recover.
                recovery.recover_copy(self.db, parent / 'retry')
                self.assertEqual(before, family(self.db))

    def test_br_inode_authority_alone_excludes_recovery(self):
        script = r'''
import ctypes, fcntl, os, sys
class Flock(ctypes.Structure):
    _fields_ = [('kind', ctypes.c_short), ('whence', ctypes.c_short),
                ('offset', ctypes.c_longlong), ('length', ctypes.c_longlong), ('pid', ctypes.c_int)]
fd = os.open(sys.argv[1], os.O_RDWR)
lock = Flock(fcntl.F_WRLCK, os.SEEK_SET, 2**63 - 2, 1, 0)
fcntl.fcntl(fd, fcntl.F_OFD_SETLK, bytes(lock))
print('held', flush=True)
sys.stdin.readline()
os._exit(0)
'''
        with subprocess.Popen([sys.executable, '-I', '-u', '-c', script, str(self.db)],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
            try:
                self.assertEqual(ready_line(child), 'held')
                self.assert_refused('database busy')
                self.assertFalse(any(self.root.glob('.br-wal-copy-*')))
            finally:
                child.stdin.write('release\n')
                child.stdin.flush()
                child.wait(timeout=10)

    def test_input_path_exchange_detected_before_publication(self):
        replay = recovery.replay
        def exchange(*args):
            replay(*args)
            saved = self.root / 'original-inode.db'
            self.db.rename(saved)
            self.db.write_bytes(saved.read_bytes())
        with mock.patch.object(recovery, 'replay', side_effect=exchange):
            with self.assertRaisesRegex(recovery.RecoveryError, 'source name changed identity'):
                recovery.recover_copy(self.db, self.output)
        self.assertFalse(self.output.exists())

    def test_symlinked_sidecar_is_not_followed(self):
        unrelated = self.root / 'unrelated'
        unrelated.write_bytes(b'unrelated data')
        recovery.component(self.db, '-shm').symlink_to(unrelated)
        self.assert_refused('Too many levels|symbolic link')
        self.assertEqual(unrelated.read_bytes(), b'unrelated data')


    def test_restore_missing_shm_preserves_existing_files_and_same_database_identity(self):
        before = family(self.db)
        modes = {suffix: recovery.metadata(recovery.component(self.db, suffix).stat())
                 for suffix, data in before.items() if data is not None}
        report = recovery.recover_copy(self.db, self.output, restore_shm=True)
        after = family(self.db)
        self.assertEqual([suffix for suffix in before if before[suffix] != after[suffix]], ['-shm'])
        self.assertEqual(Path(report['database_path']), self.db)
        self.assertTrue(report['live_recovery'])
        self.assertTrue(report['existing_source_files_unchanged'])
        self.assertFalse(report['source_unchanged'])
        self.assertFalse(report['verified_index']['checkpoint_performed'])
        self.assertEqual(after['-shm'], (self.output / 'verified-shm').read_bytes())
        self.assertEqual(recovery.component(self.db, '-shm').stat().st_mode & 0o777, 0o600)
        self.assertEqual(report, json.loads((self.output / 'complete.json').read_text()))
        self.assertTrue((self.output / 'shm-install-complete.json').is_file())
        for suffix, expected in modes.items():
            self.assertEqual(recovery.metadata(recovery.component(self.db, suffix).stat()), expected)
        # Force consumption of the restored index; no implicit SHM recovery.
        with recovery.open_regular(recovery.component(self.db, '-shm'), writable=True) as pin:
            recovery.ofd_lock(pin, recovery.fcntl.F_RDLCK, 128, 1)
            recovery.ofd_lock(pin, recovery.fcntl.F_WRLCK, 120, 3)
            self.assertEqual(recovery.logical_witness(self.db, immutable=False), report['logical'])
        self.assertEqual(before['-wal'], recovery.component(self.db, '-wal').read_bytes())
        self.assertEqual(rows(self.db, 'SELECT status FROM issues'), [('open',)])
        # Deliberate post-repair write to this disposable original, not its copy.
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            self.assertEqual(conn.execute('SELECT status,closed_at,close_reason FROM issues').fetchall(),
                             [('closed', STAMP, REASON)])
            conn.execute("INSERT INTO audit VALUES ('write on repaired source')")
            conn.commit()
        self.assertEqual(rows(self.db, 'SELECT COUNT(*) FROM audit'), [(2,)])
        self.assertEqual(rows(self.db, 'SELECT status,closed_at,close_reason FROM issues'),
                         [('closed', STAMP, REASON)])
        # The original WAL evidence survives even after the stock writer checkpoints.
        self.assertEqual((self.output / 'before' / 'beads.db-wal').read_bytes(), before['-wal'])

    def test_restore_shm_page_sizes_endianness_and_committed_shrink(self):
        for size, big in ((512, False), (512, True), (65536, False), (65536, True)):
            with self.subTest(page_size=size, big_endian=big):
                root = self.root / f'{size}-{big}'
                root.mkdir()
                db = fixture(root, page_size=size, shrink=True)
                wal = recovery.component(db, '-wal')
                data = bytearray(wal.read_bytes())
                rewrite_checksums(data, big_endian=big)
                wal.write_bytes(data)
                before = family(db)
                report = recovery.recover_copy(db, root / 'out', restore_shm=True)
                self.assertEqual(report['verified_index']['byte_order'], sys.byteorder)
                self.assertEqual(report['wal']['checksum_big_endian'], big)
                self.assertEqual(rows(db, 'SELECT COUNT(*) FROM extra', immutable=False), [(2,)])
                self.assertEqual(rows(db, 'SELECT status FROM issues', immutable=False), [('closed',)])
                after = family(db)
                self.assertEqual({k: v for k, v in after.items() if k != '-shm'},
                                 {k: v for k, v in before.items() if k != '-shm'})

    def test_restore_never_replaces_an_existing_index_even_when_short(self):
        for length in (0, 1, 64, 32768):
            with self.subTest(length=length):
                root = self.root / str(length)
                root.mkdir()
                db = fixture(root)
                recovery.component(db, '-shm').write_bytes(b'x' * length)
                before = family(db)
                with self.assertRaisesRegex(recovery.RecoveryError, 'SHM already exists'):
                    recovery.recover_copy(db, root / 'out', restore_shm=True)
                self.assertEqual(family(db), before)
                self.assertFalse((root / 'out').exists())
                self.assertFalse(any(root.glob('.br-wal-copy-*')))

    def test_restore_refuses_incomplete_or_shared_namespace(self):
        for case in ('incomplete', 'shared-main', 'shared-namespace'):
            with self.subTest(case=case):
                root = self.root / case
                root.mkdir()
                db = fixture(root)
                gate = recovery.component(db, '-fsqlite-ns-gate')
                use = recovery.component(db, '-fsqlite-ns-use')
                if case == 'shared-main':
                    db.chmod(0o660)
                else:
                    gate.write_bytes(b'namespace gate')
                    if case == 'shared-namespace':
                        use.write_bytes(b'namespace use')
                        use.chmod(0o660)
                before = family(db)
                with self.assertRaisesRegex(recovery.RecoveryError, 'incomplete|shared'):
                    recovery.recover_copy(db, root / 'out', restore_shm=True)
                self.assertEqual(before, family(db))
                self.assertFalse((root / 'out').exists())

    def test_restore_preserves_namespace_and_certificate_evidence(self):
        for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use', '-wal-cert', '-wal-cert-head'):
            recovery.component(self.db, suffix).write_bytes(b'opaque evidence ' + suffix.encode())
        before = family(self.db)
        recovery.recover_copy(self.db, self.output, restore_shm=True)
        for suffix, data in before.items():
            if suffix != '-shm':
                self.assertEqual(family(self.db)[suffix], data)
            if data is not None:
                self.assertEqual(recovery.component(self.output / 'before' / 'beads.db', suffix).read_bytes(), data)

    def test_restore_rejects_private_index_header_and_page_hash_corruption(self):
        witness = recovery.logical_witness
        for offset in (12, 40, 96, 136, 16384):
            with self.subTest(offset=offset):
                def corrupt(db, *, immutable):
                    result = witness(db, immutable=immutable)
                    if not immutable and db.parent.name == 'oracle':
                        path = recovery.component(db, '-shm')
                        with path.open('r+b') as file:
                            file.seek(offset)
                            old = file.read(1)
                            file.seek(offset)
                            file.write(bytes([old[0] ^ 1]))
                    return result
                with mock.patch.object(recovery, 'logical_witness', side_effect=corrupt):
                    self.assert_refused('private SHM', restore_shm=True)

    def test_transferred_index_rehearsal_cannot_silently_repair_invalid_headers(self):
        copy = recovery.copy_new
        def corrupt_transfer(source, destination, **kwargs):
            result = copy(source, destination, **kwargs)
            if destination.parent.name == 'index-rehearsal' and destination.name.endswith('-shm'):
                with destination.open('r+b') as file:
                    file.seek(12)
                    file.write(b'\0')
            return result
        with mock.patch.object(recovery, 'copy_new', side_effect=corrupt_transfer):
            self.assert_refused('locked', restore_shm=True)

    def test_restore_rejects_private_reader_wal_rewrite(self):
        witness = recovery.logical_witness
        def change_wal(db, *, immutable):
            result = witness(db, immutable=immutable)
            if not immutable and db.parent.name == 'oracle':
                with recovery.component(db, '-wal').open('ab') as file:
                    file.write(b'private engine changed WAL')
            return result
        with mock.patch.object(recovery, 'logical_witness', side_effect=change_wal):
            self.assert_refused('private SQLite reader changed protected component', restore_shm=True)

    def test_restore_large_real_wal_crosses_both_hash_segment_boundaries(self):
        script = r'''
import os, sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
conn.execute('PRAGMA wal_autocheckpoint=0')
conn.execute('PRAGMA synchronous=OFF')  # Disposable process-crash fixture, not power-loss evidence.
conn.execute('PRAGMA wal_checkpoint(TRUNCATE)')
for n in range(8200):
    conn.execute('UPDATE extra SET n=? WHERE id=1', (n,))
    conn.commit()
os._exit(0)
'''
        subprocess.run([sys.executable, '-I', '-c', script, str(self.db)], check=True, timeout=20)
        copy_dir = self.root / 'large-missing'
        copy_dir.mkdir()
        db = copy_dir / 'beads.db'
        for suffix, data in family(self.db).items():
            if data is not None and suffix != '-shm':
                recovery.component(db, suffix).write_bytes(data)
        before = family(db)
        report = recovery.recover_copy(db, self.output, restore_shm=True)
        self.assertGreater(report['wal']['frame_count'], 8158)
        self.assertGreaterEqual(report['verified_index']['segments'], 3)
        with recovery.open_regular(recovery.component(db, '-shm'), writable=True) as pin:
            recovery.ofd_lock(pin, recovery.fcntl.F_RDLCK, 128, 1)
            recovery.ofd_lock(pin, recovery.fcntl.F_WRLCK, 120, 3)
            self.assertEqual(rows(db, 'SELECT n FROM extra WHERE id=1', immutable=False), [(8199,)])
        self.assertEqual(before['-wal'], recovery.component(db, '-wal').read_bytes())
        self.assertEqual(before[''], db.read_bytes())

    def test_restore_refuses_live_namespace_peers_in_both_lock_families(self):
        for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use'):
            recovery.component(self.db, suffix).write_bytes(b'retained namespace')
        script = r'''
import fcntl, os, sys
file = open(sys.argv[1], 'rb')
if sys.argv[2] == 'flock':
    fcntl.flock(file, fcntl.LOCK_SH | fcntl.LOCK_NB)
else:
    fcntl.lockf(file, fcntl.LOCK_SH | fcntl.LOCK_NB)
print('held', flush=True)
sys.stdin.readline()
os._exit(0)
'''
        for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use'):
            for kind in ('flock', 'posix'):
                with self.subTest(suffix=suffix, kind=kind):
                    path = recovery.component(self.db, suffix)
                    with subprocess.Popen([sys.executable, '-I', '-u', '-c', script, str(path), kind],
                                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
                        try:
                            self.assertEqual(ready_line(child), 'held')
                            self.assert_refused('engine namespace busy', restore_shm=True)
                            self.assertFalse(any(self.root.glob('.br-wal-copy-*')))
                        finally:
                            child.stdin.write('release\n')
                            child.stdin.flush()
                            child.wait(timeout=10)

    def test_restore_excludes_arriving_users_through_install_and_completion(self):
        for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use'):
            recovery.component(self.db, suffix).write_bytes(b'retained namespace')
        script = r'''
import errno, fcntl, sqlite3, sys
path, kind = sys.argv[1:]
if kind == 'sqlite':
    conn = sqlite3.connect(path, timeout=0)
    try:
        conn.execute("UPDATE issues SET status='lost'")
        conn.commit()
    except sqlite3.OperationalError as error:
        assert error.sqlite_errorcode == sqlite3.SQLITE_BUSY
        sys.exit(42)
else:
    with open(path, 'r+b') as file:
        try:
            if kind == 'flock':
                fcntl.flock(file, fcntl.LOCK_SH | fcntl.LOCK_NB)
            else:
                fcntl.lockf(file, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except OSError as error:
            assert error.errno in (errno.EACCES, errno.EAGAIN)
            sys.exit(42)
sys.exit(99)
'''
        def probe():
            targets = [(self.db, 'sqlite')]
            for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use'):
                targets.extend((recovery.component(self.db, suffix), kind)
                               for kind in ('flock', 'posix'))
            for path, kind in targets:
                path.read_bytes()  # Unrelated closes must not drop our locks.
                result = subprocess.run([sys.executable, '-I', '-c', script, str(path), kind],
                                        capture_output=True, text=True, timeout=10, check=False)
                self.assertEqual(result.returncode, 42, f'{kind}: {result.stderr}')
        publish = recovery.publish_no_replace
        write_json = recovery.write_json
        stages = []
        def before_install(staged, destination):
            if destination == recovery.component(self.db, '-shm'):
                self.assertTrue((self.output / 'verified.json').is_file())
                self.assertTrue((self.output / 'shm-install-prepared.json').is_file())
                probe()
                stages.append('install')
            publish(staged, destination)
        def before_complete(path, report):
            if path == self.output / 'complete.json':
                self.assertTrue(recovery.component(self.db, '-shm').is_file())
                probe()
                stages.append('complete')
            write_json(path, report)
        with mock.patch.object(recovery, 'publish_no_replace', side_effect=before_install), \
                mock.patch.object(recovery, 'write_json', side_effect=before_complete):
            recovery.recover_copy(self.db, self.output, restore_shm=True)
        self.assertEqual(stages, ['install', 'complete'])

    def test_restore_refuses_replaced_namespace_inode_before_capture(self):
        for suffix in ('-fsqlite-ns-gate', '-fsqlite-ns-use'):
            recovery.component(self.db, suffix).write_bytes(b'retained namespace')
        capture = recovery.capture
        def exchange(*args, **kwargs):
            path = recovery.component(self.db, '-fsqlite-ns-use')
            path.rename(self.root / 'old-namespace')
            path.write_bytes(b'retained namespace')
            return capture(*args, **kwargs)
        with mock.patch.object(recovery, 'capture', side_effect=exchange):
            with self.assertRaisesRegex(recovery.RecoveryError, 'source name changed identity'):
                recovery.recover_copy(self.db, self.output, restore_shm=True)
        self.assertFalse(recovery.component(self.db, '-shm').exists())
        self.assertFalse(self.output.exists())

    def test_restore_atomic_install_never_overwrites_a_racing_index(self):
        before = family(self.db)
        publish = recovery.publish_no_replace
        def race(staged, destination):
            if destination == recovery.component(self.db, '-shm'):
                destination.write_bytes(b'noncooperating racing index')
            publish(staged, destination)
        with mock.patch.object(recovery, 'publish_no_replace', side_effect=race):
            with self.assertRaisesRegex(recovery.RecoveryError, 'File exists'):
                recovery.recover_copy(self.db, self.output, restore_shm=True)
        after = family(self.db)
        self.assertEqual(after['-shm'], b'noncooperating racing index')
        for suffix in before:
            if suffix != '-shm':
                self.assertEqual(before[suffix], after[suffix])
        self.assertEqual((self.output / 'before/beads.db-wal').read_bytes(), before['-wal'])
        self.assertTrue((self.output / 'failed.json').exists())
        self.assertFalse((self.output / 'complete.json').exists())

    def test_restore_sync_failure_never_rolls_back_an_installed_index(self):
        before = family(self.db)
        sync = recovery.sync_directory
        def fail_after_install(path):
            if path == self.db.parent and recovery.component(self.db, '-shm').exists():
                raise OSError('injected post-install sync failure')
            sync(path)
        with mock.patch.object(recovery, 'sync_directory', side_effect=fail_after_install):
            with self.assertRaisesRegex(recovery.RecoveryError, 'post-install sync failure'):
                recovery.recover_copy(self.db, self.output, restore_shm=True)
        self.assertEqual(recovery.component(self.db, '-shm').read_bytes(),
                         (self.output / 'verified-shm').read_bytes())
        for suffix, data in before.items():
            if suffix != '-shm':
                self.assertEqual(family(self.db)[suffix], data)
        self.assertTrue((self.output / 'failed.json').is_file())
        self.assertFalse((self.output / 'complete.json').exists())
        with recovery.open_regular(recovery.component(self.db, '-shm'), writable=True) as pin:
            recovery.ofd_lock(pin, recovery.fcntl.F_RDLCK, 128, 1)
            recovery.ofd_lock(pin, recovery.fcntl.F_WRLCK, 120, 3)
            self.assertEqual(rows(self.db, 'SELECT status FROM issues', immutable=False), [('closed',)])

    def test_restore_process_death_leaves_absent_or_complete_index_and_all_wal_bytes(self):
        script = r'''
import importlib.util, os, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('recovery', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
source, output = Path(sys.argv[2]), Path(sys.argv[3])
stage = sys.argv[4]
publish = module.publish_no_replace
def crash(staging, destination):
    if destination == module.component(source, '-shm'):
        assert (output / 'before' / 'beads.db-wal').is_file()
        assert (output / 'shm-install-prepared.json').is_file()
        if stage == 'before-install':
            os._exit(73)
        publish(staging, destination)
        os._exit(73)
    publish(staging, destination)
module.publish_no_replace = crash
module.recover_copy(source, output, restore_shm=True)
raise AssertionError('crash injection was not reached')
'''
        for stage in ('before-install', 'after-install'):
            with self.subTest(stage=stage):
                root = self.root / stage
                root.mkdir()
                db = fixture(root)
                before = family(db)
                output = root / 'out'
                result = subprocess.run([sys.executable, '-I', '-c', script, str(SCRIPT),
                                         str(db), str(output), stage], capture_output=True,
                                        text=True, timeout=20, check=False)
                self.assertEqual(result.returncode, 73, result.stderr)
                after = family(db)
                for suffix in before:
                    if suffix != '-shm':
                        self.assertEqual(before[suffix], after[suffix])
                self.assertEqual((output / 'before/beads.db-wal').read_bytes(), before['-wal'])
                self.assertFalse((output / 'complete.json').exists())
                self.assertFalse((output / 'shm-install-complete.json').exists())
                if stage == 'before-install':
                    self.assertIsNone(after['-shm'])
                    recovery.recover_copy(db, root / 'retry', restore_shm=True)
                else:
                    self.assertEqual(after['-shm'], (output / 'verified-shm').read_bytes())
                    with self.assertRaisesRegex(recovery.RecoveryError, 'SHM already exists'):
                        recovery.recover_copy(db, root / 'retry', restore_shm=True)
                with recovery.open_regular(recovery.component(db, '-shm'), writable=True) as pin:
                    recovery.ofd_lock(pin, recovery.fcntl.F_RDLCK, 128, 1)
                    recovery.ofd_lock(pin, recovery.fcntl.F_WRLCK, 120, 3)
                    self.assertEqual(rows(db, 'SELECT status FROM issues', immutable=False), [('closed',)])

    def test_restore_cli_names_the_original_and_preserves_all_existing_files(self):
        before = family(self.db)
        result = subprocess.run([sys.executable, str(SCRIPT), '--db', str(self.db),
                                 '--output', str(self.output), '--restore-shm'],
                                capture_output=True, text=True, timeout=20, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report['ok'])
        self.assertTrue(report['existing_source_files_unchanged'])
        self.assertEqual(Path(report['database_path']), self.db)
        self.assertEqual(Path(report['recovered_copy_path']), self.output / 'beads.db')
        for suffix in before:
            if suffix != '-shm':
                self.assertEqual(before[suffix], family(self.db)[suffix])


def qualify_br(br: Path) -> None:
    """Run only on an explicitly supplied real binary; never substitute SQLite."""
    spec = importlib.util.spec_from_file_location('fixture_556', SCRIPT.parents[1] /
                                                'tests/e2e_scripts/wal_recovery_556.py')
    base = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(base)
    br = br.resolve(strict=True)
    recovery.require(br.is_file() and os.access(br, os.X_OK), 'not an executable br')
    root = Path(tempfile.mkdtemp(prefix='br-wal-copy-br-'))
    print(f'Retaining actual-br recovery qualification at {root}', flush=True)
    seed = base.setup_seed(br, root / 'seed')
    ambient = base.setup_seed(br, root / 'ambient')
    orphan = root / 'orphan' / 'selected.db'
    base.retain(family(seed), orphan)
    issue_id = base.orphaned_closure(orphan)
    missing = family(orphan)
    missing['-shm'] = None
    source = root / 'source' / "-selected ' λ.db"
    base.retain(missing, source)
    ambient_before = family(ambient)
    ambient_artifacts = base.recovery_files(ambient.parent)
    output = root / 'recovered'
    result = subprocess.run([sys.executable, str(SCRIPT), '--db', str(source), '--output', str(output)],
                            capture_output=True, text=True, timeout=60, check=False)
    (root / 'recovery.stdout').write_text(result.stdout)
    (root / 'recovery.stderr').write_text(result.stderr)
    recovery.require(result.returncode == 0, f'copy recovery failed: {result.stderr}')
    report = json.loads(result.stdout)
    db = Path(report['database_path'])
    flags = ['--db', str(db), '--no-auto-import', '--no-auto-flush']
    listed = base.run_br(br, root, ambient.parent, [*flags, 'list', '--all', '--json'], 'list-before')
    recovery.require(listed.returncode == 0, f'recovered br list failed: {listed.stderr}')
    issues = json.loads(listed.stdout)['issues']
    recovery.require(any(row['id'] == issue_id and row['status'] == 'closed' for row in issues),
                     'br did not observe the recovered WAL-only closure')
    created = base.run_br(br, root, ambient.parent,
                          [*flags, 'create', 'GH562 deliberate post-copy write', '--json'], 'create-after')
    recovery.require(created.returncode == 0, f'recovered br write failed: {created.stderr}')
    listed = base.run_br(br, root, ambient.parent, [*flags, 'list', '--all', '--json'], 'list-after')
    recovery.require(listed.returncode == 0, f'post-write br list failed: {listed.stderr}')
    recovery.require(any(row['title'] == 'GH562 deliberate post-copy write'
                         for row in json.loads(listed.stdout)['issues']), 'br write did not persist')
    recovery.require(rows(db, 'SELECT status,closed_at,close_reason FROM issues WHERE id=' +
                          "'" + issue_id.replace("'", "''") + "'", immutable=False) ==
                     [('closed', base.STAMP, base.REASON)], 'br write lost the WAL-only closure')
    recovery.require(family(source) == missing, 'source family changed')
    recovery.require(family(ambient) == ambient_before, 'ambient database changed')
    recovery.require(base.recovery_files(ambient.parent) == ambient_artifacts, 'ambient artifacts changed')
    print('Actual br: recovered closure, successful new write, and unchanged source/ambient verified.')

    # Qualify same-path restoration separately. The original is still missing
    # SHM here. Do NOT let stock SQLite open it before br: that would repair
    # the index itself and hide whether FrankenSQLite consumed our template.
    identity = source.stat().st_dev, source.stat().st_ino
    restored_output = root / 'same-path-evidence'
    result = subprocess.run([sys.executable, str(SCRIPT), '--db', str(source),
                             '--output', str(restored_output), '--restore-shm'],
                            capture_output=True, text=True, timeout=60, check=False)
    (root / 'restore-shm.stdout').write_text(result.stdout)
    (root / 'restore-shm.stderr').write_text(result.stderr)
    recovery.require(result.returncode == 0, f'SHM restoration failed: {result.stderr}')
    report = json.loads(result.stdout)
    recovery.require(Path(report['database_path']) == source and report['live_recovery'],
                     'restoration did not target the original database')
    recovery.require((source.stat().st_dev, source.stat().st_ino) == identity, 'source inode changed')
    restored = family(source)
    recovery.require(restored['-shm'] == (restored_output / 'verified-shm').read_bytes(),
                     'installed index differs from the verified template')
    recovery.require(all(restored[suffix] == data for suffix, data in missing.items() if suffix != '-shm'),
                     'SHM restoration changed an existing database component')
    flags = ['--db', str(source), '--no-auto-import', '--no-auto-flush']
    listed = base.run_br(br, root, ambient.parent, [*flags, 'list', '--all', '--json'], 'restored-list')
    recovery.require(listed.returncode == 0, f'br could not read restored SHM: {listed.stderr}')
    recovery.require(any(row['id'] == issue_id and row['status'] == 'closed'
                         for row in json.loads(listed.stdout)['issues']), 'restored br read lost the closure')
    recovery.require(source.read_bytes() == missing[''] and
                     recovery.component(source, '-wal').read_bytes() == missing['-wal'],
                     'initial br read changed protected main/WAL bytes')
    created = base.run_br(br, root, ambient.parent,
                          [*flags, 'create', 'GH562 same-path restored write', '--json'], 'restored-create')
    recovery.require(created.returncode == 0, f'br could not write after SHM restoration: {created.stderr}')
    listed = base.run_br(br, root, ambient.parent, [*flags, 'list', '--all', '--json'], 'restored-list-after')
    recovery.require(listed.returncode == 0, f'restored post-write list failed: {listed.stderr}')
    recovery.require(any(row['title'] == 'GH562 same-path restored write'
                         for row in json.loads(listed.stdout)['issues']), 'same-path br write did not persist')
    recovery.require(rows(source, 'SELECT status,closed_at,close_reason FROM issues WHERE id=' +
                          "'" + issue_id.replace("'", "''") + "'", immutable=False) ==
                     [('closed', base.STAMP, base.REASON)], 'same-path br write lost the committed closure')
    recovery.require(family(ambient) == ambient_before, 'same-path recovery changed ambient data')
    recovery.require(base.recovery_files(ambient.parent) == ambient_artifacts, 'ambient artifacts changed')
    print('Actual br: same-inode SHM restoration, committed closure, and subsequent write verified.')


if __name__ == '__main__':
    if '--br' in sys.argv[1:]:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument('--br', type=Path, required=True)
        qualify_br(parser.parse_args().br)
    else:
        unittest.main(verbosity=2)
