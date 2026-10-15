#!/usr/bin/env python3
"""Recover an abandoned WAL transaction on NEW disposable database families.

The default mode exercises the production Python helper with real SQLite
cache-spill/crash fixtures. --recovery-binary exercises an actual compiled
companion; it never falls back to the helper. All evidence is retained.
"""
from __future__ import annotations

import argparse
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from test_wal_recovery_resume import reader

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts' / 'recover_orphaned_wal.py'
SPEC = importlib.util.spec_from_file_location('committed_copy_recovery', SCRIPT)
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)
NATIVE = None
STAMP = '2026-10-14T00:00:00Z'
REASON = 'committed WAL closure before abandoned transaction'


def family(db):
    return {suffix: recovery.component(db, suffix).read_bytes()
            if recovery.component(db, suffix).exists() else None
            for suffix in recovery.SUFFIXES}


def identities(db):
    return {suffix: recovery.metadata(recovery.component(db, suffix).stat())
            if recovery.component(db, suffix).exists() else None
            for suffix in recovery.SUFFIXES}


def retain(state, db):
    db.parent.mkdir(parents=True, exist_ok=True)
    for suffix, data in state.items():
        if data is not None:
            with recovery.component(db, suffix).open('xb') as out:
                out.write(data)


def fixture(root, page_size=512, commits=True):
    seed = root / 'seed.db'
    program = r'''
import json, os, sqlite3, sys
path, page_size, commits, stamp, reason = sys.argv[1:]
size = int(page_size)
conn = sqlite3.connect(path)
conn.execute('PRAGMA page_size=' + page_size)
assert conn.execute('PRAGMA journal_mode=WAL').fetchone()[0] == 'wal'
conn.execute('PRAGMA wal_autocheckpoint=0')
conn.execute('PRAGMA cache_size=5')
conn.execute('PRAGMA cache_spill=ON')
conn.execute('CREATE TABLE issues (id INTEGER PRIMARY KEY, status TEXT, closed_at TEXT, close_reason TEXT, payload BLOB)')
blob_size = max(4096, size * 2)
conn.executemany("INSERT INTO issues VALUES (?, 'open', NULL, '', ?)",
                 [(i, b'O' * blob_size) for i in range(1, 25)])
conn.commit()
assert conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0, 0, 0)
if commits == 'yes':
    conn.execute("UPDATE issues SET status='closed', closed_at=?, close_reason=?, payload=? WHERE id=1",
                 (stamp, reason, b'C' * blob_size))
    conn.commit()
    conn.execute("UPDATE issues SET payload=? WHERE id=2", (b'B' * blob_size,))
    conn.commit()
committed_length = os.path.getsize(path + '-wal')
conn.execute('BEGIN IMMEDIATE')
conn.execute("UPDATE issues SET status='uncommitted', payload=?", (b'T' * blob_size,))
conn.execute("INSERT INTO issues VALUES (99, 'uncommitted insert', NULL, '', ?)", (b'N' * blob_size,))
length = os.path.getsize(path + '-wal')
assert length > max(32, committed_length)
print(json.dumps(dict(committed_length=committed_length, length=length, blob_size=blob_size)), flush=True)
os._exit(0)  # Neither rollback nor close-time checkpoint may hide the spill.
'''
    result = subprocess.run([sys.executable, '-I', '-S', '-c', program, str(seed),
                             str(page_size), 'yes' if commits else 'no', STAMP, REASON],
                            capture_output=True, text=True, timeout=20, check=True)
    data = json.loads(result.stdout)
    state = family(seed)
    state['-shm'] = None
    db = root / "source ' $HOME λ.db"
    retain(state, db)
    frame_size = page_size + 24
    frames = [struct.unpack_from('>6I', state['-wal'], offset)
              for offset in range(32, len(state['-wal']), frame_size)]
    assert frames and frames[-1][1] == 0, 'fixture did not leave uncommitted WAL frames'
    assert bool(any(frame[1] for frame in frames)) == commits
    if commits:
        data['commit_offsets'] = [32 + (i + 1) * frame_size
                                  for i, frame in enumerate(frames) if frame[1]]
        last_commit = max(i + 1 for i, frame in enumerate(frames) if frame[1])
        assert data['committed_length'] == 32 + last_commit * frame_size
        # Guarantee the bug discriminator: later uncommitted images overwrite
        # pages already represented in the committed prefix.
        assert {f[0] for f in frames[:last_commit]} & {f[0] for f in frames[last_commit:]}
    return db, data


class CommittedCopyTests(unittest.TestCase):
    def setUp(self):
        recovery.platform_check()
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-committed-copy-'))
        self.db, self.info = fixture(self.root)
        self.output = self.root / 'recovered'

    def invoke(self, *, enabled=True, **kwargs):
        if NATIVE is None:
            return recovery.recover_copy(self.db, self.output,
                                         copy_last_commit=enabled, **kwargs)
        argv = [str(NATIVE), '--db', str(self.db), '--output', str(self.output)]
        if enabled:
            argv.append('--copy-last-commit')
        for key, value in kwargs.items():
            argv.extend(['--' + key.replace('_', '-'), str(value)])
        result = subprocess.run(argv, capture_output=True, text=True, timeout=60, check=False)
        (self.root / 'native.stdout').write_text(result.stdout)
        (self.root / 'native.stderr').write_text(result.stderr)
        if result.returncode:
            raise recovery.RecoveryError(result.stderr)
        return json.loads(result.stdout)

    def assert_refused(self, pattern, *, enabled=True, **kwargs):
        before, ids = family(self.db), identities(self.db)
        with self.assertRaisesRegex(recovery.RecoveryError, pattern):
            self.invoke(enabled=enabled, **kwargs)
        self.assertEqual(family(self.db), before)
        self.assertEqual(identities(self.db), ids)
        self.assertFalse((self.output / 'complete.json').exists())

    def assert_copy(self, second_payload=b'B'):
        before, ids = family(self.db), identities(self.db)
        oracle = self.root / 'independent' / 'oracle.db'
        retain(before, oracle)
        with contextlib.closing(sqlite3.connect(oracle.as_uri() + '?mode=ro', uri=True)) as conn:
            expected = conn.execute('SELECT * FROM issues ORDER BY id').fetchall()
        self.assertEqual(expected[0][1:4], ('closed', STAMP, REASON))
        self.assertEqual(expected[0][4], b'C' * self.info['blob_size'])
        self.assertEqual(expected[1][4], second_payload * self.info['blob_size'])
        report = self.invoke()
        self.assertTrue(report['source_unchanged'])
        self.assertFalse(report['live_recovery'])
        self.assertTrue(report['copy_last_commit_requested'])
        self.assertEqual(report['recovery_scope'], 'last-validated-commit')
        tail = report['wal']['excluded_tail']
        self.assertEqual(tail['offset'], self.info['committed_length'])
        self.assertEqual(tail['bytes'], len(before['-wal']) - tail['offset'])
        self.assertTrue(tail['source_bytes_retained'])
        copy = Path(report['database_path'])
        with contextlib.closing(sqlite3.connect(copy.as_uri() + '?immutable=1', uri=True)) as conn:
            self.assertEqual(conn.execute('PRAGMA integrity_check').fetchall(), [('ok',)])
            self.assertEqual(conn.execute('SELECT * FROM issues ORDER BY id').fetchall(), expected)
        # Both the input and every captured byte, including the excluded tail,
        # remain available; only the NEW database omits the uncommitted images.
        self.assertEqual(family(self.db), before)
        self.assertEqual(identities(self.db), ids)
        self.assertEqual(family(self.output / 'before' / 'beads.db'), before)
        self.assertEqual(json.loads((self.output / 'complete.json').read_text())['wal'], report['wal'])
        return report

    def test_real_abandoned_spill_keeps_both_commits_without_uncommitted_overwrites(self):
        report = self.assert_copy()
        tail = report['wal']['excluded_tail']
        self.assertGreater(tail['physical_complete_frames'], 0)
        self.assertEqual(tail['partial_frame_bytes'], 0)
        self.assertEqual(report['wal']['commits'], 2)
        # SQLite defers rewritten spill-frame checksums until COMMIT. This
        # untouched real crash fixture must exercise the invalid-tail path.
        self.assertEqual(tail['validation_stopped'], 'checksum-mismatch')
        self.assertGreater(tail['unverified_bytes'], 0)

    def test_strict_mode_still_refuses_uncommitted_tail(self):
        self.assert_refused('checksum', enabled=False)

    def test_torn_last_frame_is_preserved_but_not_replayed(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.truncate(wal.stat().st_size - 101)
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['partial_frame_bytes'], 512 + 24 - 101)

    def test_torn_tail_does_not_relax_default_recovery(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.truncate(wal.stat().st_size - 1)
        self.assert_refused('partial frame', enabled=False)

    def test_partial_frame_immediately_after_commit(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.truncate(self.info['committed_length'] + 17)
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['physical_complete_frames'], 0)
        self.assertEqual(report['wal']['excluded_tail']['partial_frame_bytes'], 17)
        self.assertEqual(report['wal']['excluded_tail']['validation_stopped'], 'partial-frame')

    def test_copy_stops_before_a_corrupt_complete_tail_frame(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(self.info['committed_length'] + 24)
            byte = file.read(1)
            file.seek(-1, os.SEEK_CUR)
            file.write(bytes([byte[0] ^ 1]))
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['validation_stopped'], 'checksum-mismatch')
        self.assertEqual(report['wal']['excluded_tail']['validated_uncommitted_frames'], 0)

    def test_copy_stops_at_old_generation_tail(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(self.info['committed_length'] + 8)
            file.write(b'othergen')
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['validation_stopped'], 'salt-mismatch')

    def test_no_validated_commit_is_refused(self):
        root = self.root / 'no-commit'
        root.mkdir()
        self.db, self.info = fixture(root, commits=False)
        self.assert_refused('no committed WAL frame')

    def test_all_committed_wal_remains_compatible(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.truncate(self.info['committed_length'])
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['bytes'], 0)
        self.output = self.root / 'strict-complete'
        strict = self.invoke(enabled=False)
        self.assertNotIn('copy_last_commit_requested', strict)
        self.assertNotIn('excluded_tail', strict['wal'])
        self.assertEqual(strict['recovered_sha256'], report['recovered_sha256'])
        self.assertEqual(strict['wal'], {k: v for k, v in report['wal'].items() if k != 'excluded_tail'})

    def test_65536_byte_pages(self):
        root = self.root / 'large-pages'
        root.mkdir()
        self.db, self.info = fixture(root, page_size=65536)
        report = self.assert_copy()
        self.assertEqual(report['wal']['page_size'], 65536)

    def test_recovered_database_supports_subsequent_writes(self):
        report = self.assert_copy()
        with contextlib.closing(sqlite3.connect(report['database_path'])) as conn:
            conn.execute("INSERT INTO issues VALUES (100, 'new write', NULL, '', X'ff')")
            conn.commit()
            self.assertEqual(conn.execute('SELECT status,closed_at,close_reason FROM issues WHERE id=1').fetchone(),
                             ('closed', STAMP, REASON))
            self.assertEqual(conn.execute('SELECT count(*) FROM issues WHERE id=99').fetchone(), (0,))
            self.assertEqual(conn.execute('PRAGMA integrity_check').fetchone(), ('ok',))

    def test_mode_cannot_authorize_restore_or_resume(self):
        for kwargs in ({'restore_shm': True}, {'resume_from': self.root / 'absent-evidence'}):
            with self.subTest(kwargs=kwargs):
                if NATIVE is not None:
                    before, ids = family(self.db), identities(self.db)
                    option = (['--restore-shm'] if 'restore_shm' in kwargs else
                              ['--resume-from', str(kwargs['resume_from'])])
                    result = subprocess.run([str(NATIVE), '--db', str(self.db), '--output',
                                             str(self.output), '--copy-last-commit', *option],
                                            capture_output=True, text=True, timeout=30, check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(self.output.exists())
                    self.assertEqual(family(self.db), before)
                    self.assertEqual(identities(self.db), ids)
                    continue
                with mock.patch.object(recovery, 'open_regular', side_effect=AssertionError('source opened')):
                    with self.assertRaisesRegex(recovery.RecoveryError, 'copy-only'):
                        recovery.recover_copy(self.db, self.output, copy_last_commit=True, **kwargs)

    def test_existing_destination_is_not_replaced(self):
        self.output.mkdir()
        (self.output / 'sentinel').write_bytes(b'keep')
        self.assert_refused('destination already exists')
        self.assertEqual((self.output / 'sentinel').read_bytes(), b'keep')

    def test_full_tail_is_subject_to_byte_bound(self):
        limit = max(self.db.stat().st_size, self.info['committed_length'])
        self.assertGreater(recovery.component(self.db, '-wal').stat().st_size, limit)
        self.assert_refused('exceeds|--max-bytes', max_bytes=limit)

    def test_corruption_before_first_commit_never_produces_a_candidate(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(32 + 24)
            original = file.read(1)
            file.seek(-1, os.SEEK_CUR)
            file.write(bytes([original[0] ^ 0x80]))
        self.assert_refused('no committed WAL frame')

    def test_bad_header_is_not_prefix_recovery_authority(self):
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(24)
            original = file.read(1)
            file.seek(-1, os.SEEK_CUR)
            file.write(bytes([original[0] ^ 0x80]))
        self.assert_refused('header checksum')

    def test_corrupt_later_transaction_reports_only_the_earlier_commit(self):
        # Deliberate corruption of a complete WAL commit in this disposable
        # family. Success is explicitly a prefix COPY, not recovery of all
        # historical commits. The original damaged bytes must remain intact.
        first_end = self.info['commit_offsets'][0]
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(first_end + 24)
            original = file.read(1)
            file.seek(-1, os.SEEK_CUR)
            file.write(bytes([original[0] ^ 0x80]))
        self.info['committed_length'] = first_end
        report = self.assert_copy(second_payload=b'O')
        self.assertEqual(report['wal']['commits'], 1)
        self.assertGreater(report['wal']['excluded_tail']['unverified_bytes'], 0)
        self.assertIn('later changes', report['note'])

    def test_checksum_valid_uncommitted_overwrite_does_not_leak(self):
        # Construct ONE checksummed noncommit frame using a real SQLite page
        # image. Unlike the real crash fixture, this deliberately exercises a
        # completely valid tail rather than deferred-checksum rejection.
        wal = recovery.component(self.db, '-wal')
        data = wal.read_bytes()
        end = self.info['committed_length']
        frame_size = 512 + 24
        source_frame = bytearray(data[end:end + frame_size])
        source_frame[4:8] = b'\0' * 4
        a, b = struct.unpack_from('>II', data, end - frame_size + 16)
        endian = '>' if struct.unpack_from('>I', data)[0] & 1 else '<'
        # Independent implementation of the format's rolling two-word sum.
        for chunk in (source_frame[:8], source_frame[24:]):
            words = struct.unpack(endian + 'I' * (len(chunk) // 4), chunk)
            for i in range(0, len(words), 2):
                a = (a + words[i] + b) & 0xffffffff
                b = (b + words[i + 1] + a) & 0xffffffff
        struct.pack_into('>II', source_frame, 16, a, b)
        with wal.open('r+b') as file:
            file.seek(end)
            file.write(source_frame)
            file.truncate()
        report = self.assert_copy()
        self.assertEqual(report['wal']['excluded_tail']['validation_stopped'], 'eof')
        self.assertEqual(report['wal']['excluded_tail']['validated_uncommitted_frames'], 1)
        self.assertEqual(report['wal']['excluded_tail']['unverified_bytes'], 0)
        self.output = self.root / 'strict-valid-tail'
        self.assert_refused('uncommitted WAL tail', enabled=False)

    def test_independent_oracle_rejects_dropped_wal_data(self):
        if NATIVE is not None:
            self.skipTest('in-process fault injection belongs to helper tests')
        def discard_wal(main, wal, plan, destination):
            recovery.copy_new(main, destination)
        with mock.patch.object(recovery, 'replay', side_effect=discard_wal):
            self.assert_refused('differs from the WAL-aware oracle')

    def test_command_line_emits_explicit_scope_and_tail(self):
        command = [str(NATIVE)] if NATIVE else [sys.executable, '-I', '-S', str(SCRIPT)]
        result = subprocess.run([*command, '--db', str(self.db),
                                 '--output', str(self.output), '--copy-last-commit'],
                                capture_output=True, text=True, timeout=30, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report['ok'])
        self.assertEqual(report['recovery_scope'], 'last-validated-commit')
        self.assertGreater(report['wal']['excluded_tail']['bytes'], 0)

    def test_real_reader_contention_retains_source_family(self):
        with reader(self.db):
            self.assert_refused('busy')

    def test_opposite_checksum_order_with_valid_uncommitted_tail(self):
        # Endian conformance control, not a second real engine writer. Turn
        # the real page images into a big-endian-checksum WAL and finish every
        # deferred salt/checksum without adding a commit marker to its tail.
        wal = recovery.component(self.db, '-wal')
        data = bytearray(wal.read_bytes())
        struct.pack_into('>I', data, 0, 0x377f0683)
        def cksum(block, a, b):
            words = struct.unpack('>' + 'I' * (len(block) // 4), block)
            for i in range(0, len(words), 2):
                a = (a + words[i] + b) & 0xffffffff
                b = (b + words[i + 1] + a) & 0xffffffff
            return a, b
        a, b = cksum(data[:24], 0, 0)
        struct.pack_into('>II', data, 24, a, b)
        for offset in range(32, len(data), 536):
            self.assertGreater(struct.unpack_from('>I', data, offset)[0], 0)
            data[offset + 8:offset + 16] = data[16:24]
            a, b = cksum(data[offset:offset + 8], a, b)
            a, b = cksum(data[offset + 24:offset + 536], a, b)
            struct.pack_into('>II', data, offset + 16, a, b)
        wal.write_bytes(data)
        report = self.assert_copy()
        self.assertTrue(report['wal']['checksum_big_endian'])
        self.assertEqual(report['wal']['excluded_tail']['unverified_bytes'], 0)
        self.assertGreater(report['wal']['excluded_tail']['validated_uncommitted_frames'], 1)


def qualify_br(br):
    """Use real br schema/commands; source and ambient families are never adopted."""
    import wal_recovery_556 as base

    br = br.resolve(strict=True)
    if not br.is_file() or not os.access(br, os.X_OK) or NATIVE is None:
        raise AssertionError('both actual br and the installed companion are required')
    root = Path(tempfile.mkdtemp(prefix='br-wal-committed-copy-'))
    print(f'Actual-br committed-prefix qualification retained at {root}', flush=True)
    seed = base.setup_seed(br, root / 'seed')
    ambient = base.setup_seed(br, root / 'ambient')
    orphan = root / 'orphan' / 'selected.db'
    retain(family(seed), orphan)
    issue_id = base.orphaned_closure(orphan)
    program = r'''
import os, sqlite3, sys
db, issue = sys.argv[1:]
conn = sqlite3.connect(db)
conn.execute('PRAGMA wal_autocheckpoint=0')
conn.execute('PRAGMA cache_size=5')
conn.execute('PRAGMA cache_spill=ON')
before = os.path.getsize(db + '-wal')
conn.execute('BEGIN IMMEDIATE')
conn.execute("UPDATE issues SET status='open', closed_at=NULL, close_reason='' WHERE id=?", (issue,))
conn.execute('CREATE TABLE uncommitted_copy_probe (id INTEGER PRIMARY KEY, payload BLOB)')
conn.executemany('INSERT INTO uncommitted_copy_probe VALUES (?, ?)',
                 [(i, b'uncommitted' * 4096) for i in range(32)])
assert os.path.getsize(db + '-wal') > before
os._exit(0)
'''
    subprocess.run([sys.executable, '-I', '-S', '-c', program, str(orphan), issue_id],
                   capture_output=True, text=True, timeout=30, check=True)
    original = family(orphan)
    original['-shm'] = None
    source = root / 'source' / "selected ' λ.db"
    retain(original, source)
    ambient_before = family(ambient)
    ambient_evidence = base.recovery_files(ambient.parent)
    output = root / 'candidate'
    result = subprocess.run([str(NATIVE), '--db', str(source), '--output', str(output),
                             '--copy-last-commit'], capture_output=True, text=True,
                            timeout=60, check=False)
    (root / 'native.stdout').write_text(result.stdout)
    (root / 'native.stderr').write_text(result.stderr)
    base.require(result.returncode == 0, f'companion failed: {result.stderr}')
    report = json.loads(result.stdout)
    base.require(report['recovery_scope'] == 'last-validated-commit', 'wrong recovery mode')
    base.require(report['wal']['excluded_tail']['bytes'] > 0, 'fixture had no excluded tail')
    db = Path(report['database_path'])
    flags = ['--db', str(db), '--no-auto-import', '--no-auto-flush']
    for stage in ('before', 'after'):
        listed = base.run_br(br, root, ambient.parent,
                             [*flags, 'list', '--all', '--json'], 'list-' + stage)
        base.require(listed.returncode == 0, f'br list failed: {listed.stderr}')
        rows = json.loads(listed.stdout)['issues']
        base.require(any(row['id'] == issue_id and row['status'] == 'closed' for row in rows),
                     'br lost the WAL-only closure or exposed the uncommitted update')
        if stage == 'before':
            created = base.run_br(br, root, ambient.parent,
                                 [*flags, 'create', 'post-prefix deliberate write', '--json'], 'create')
            base.require(created.returncode == 0, f'br create failed: {created.stderr}')
        else:
            base.require(any(row['title'] == 'post-prefix deliberate write' for row in rows),
                         'br did not retain the deliberate write')
    inspection = root / 'inspection' / 'beads.db'
    retain(family(db), inspection)
    with contextlib.closing(sqlite3.connect(inspection.as_uri() + '?mode=ro', uri=True)) as conn:
        base.require(base.issue_state(conn, issue_id) == ('closed', base.STAMP, base.REASON),
                     'native write lost the exact committed closure fields')
        base.require(conn.execute("SELECT count(*) FROM sqlite_schema WHERE name='uncommitted_copy_probe'").fetchone() == (0,),
                     'uncommitted schema changes leaked into the candidate')
        base.require(conn.execute('PRAGMA integrity_check').fetchall() == [('ok',)],
                     'native post-copy write failed integrity')
    base.require(family(source) == original, 'source changed')
    base.require(family(ambient) == ambient_before, 'ambient database changed')
    base.require(base.recovery_files(ambient.parent) == ambient_evidence, 'ambient evidence changed')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--recovery-binary', type=Path)
    parser.add_argument('--br', type=Path)
    args, rest = parser.parse_known_args()
    if args.recovery_binary:
        NATIVE = args.recovery_binary.resolve(strict=True)
        if not NATIVE.is_file() or not os.access(NATIVE, os.X_OK):
            raise SystemExit('an actual executable recovery companion is required')
    if args.br and NATIVE is None:
        parser.error('--br requires --recovery-binary; no helper fallback is permitted')
    print('Actual companion: ' + str(NATIVE) if NATIVE else 'Production Python helper only; not a Rust binary.', flush=True)
    result = unittest.main(argv=[sys.argv[0], *rest], verbosity=2, exit=False).result
    if not result.wasSuccessful():
        raise SystemExit(1)
    if args.br:
        qualify_br(args.br)
