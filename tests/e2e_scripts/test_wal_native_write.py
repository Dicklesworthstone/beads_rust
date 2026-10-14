#!/usr/bin/env python3
"""Qualify private native writes, independently of native read-only success.

--recovery-binary MUST be an actual installed br-wal-recover; no interpreter
or mock fallback exists. --self-test instead exercises stock-SQLite fixture
and receipt-oracle controls only. It does NOT execute or qualify Rust.
Every database and injected fault lives in a new retained temporary directory.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest

NATIVE = None
NATIVE_SCHEMA = 'br.orphaned_wal.native_probe.v1'
WRITE_SCHEMA = 'br.orphaned_wal.native_write.v1'
SUFFIXES = ('', '-wal', '-shm')
CLOSURE = ('closed', '2026-10-14T00:00:00Z', "WAL-only ' closure λ", b'\x00\xff' * 4096)


def component(db, suffix):
    return db.with_name(db.name + suffix)


def family(db):
    return {suffix: component(db, suffix).read_bytes() if component(db, suffix).exists() else None
            for suffix in SUFFIXES}


def copy_family(source, target):
    target.parent.mkdir(mode=0o700, parents=True, exist_ok=False)
    for suffix, data in family(source).items():
        if data is not None:
            with component(target, suffix).open('xb') as out:
                out.write(data)


def rows_digest(rows):
    encoded = []
    for row in rows:
        data = bytearray(struct.pack('>Q', len(row)))
        for value in row:
            if value is None:
                data += b'n'
            elif type(value) is int:
                data += b'i' + struct.pack('>q', value)
            elif type(value) is float:
                data += b'r' + struct.pack('>d', value)
            else:
                tag = b'b' if isinstance(value, bytes) else b't'
                value = value if isinstance(value, bytes) else value.encode('utf-8')
                data += tag + struct.pack('>Q', len(value)) + value
        encoded.append(bytes(data))
    digest = hashlib.sha256()
    for row in sorted(encoded):
        digest.update(struct.pack('>Q', len(row)))
        digest.update(row)
    return dict(rows=len(encoded), sha256=digest.hexdigest())


def quoted(value):
    return '"' + value.replace('"', '""') + '"'


def manifest(db, immutable=False):
    uri = db.as_uri() + ('?mode=ro&immutable=1' if immutable else '?mode=ro')
    with contextlib.closing(sqlite3.connect(uri, uri=True)) as conn:
        conn.execute('BEGIN')
        if conn.execute('PRAGMA integrity_check').fetchall() != [('ok',)]:
            raise AssertionError('private witness integrity failure')
        schema = conn.execute('SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema').fetchall()
        tables = []
        for kind, name, _, _, _ in schema:
            if kind != 'table':
                continue
            columns = [row[1] for row in conn.execute('PRAGMA table_xinfo(' + quoted(name) + ')')]
            alias = next(a for a in ('rowid', '_rowid_', 'oid') if a not in columns)
            select = alias + ',' + ','.join(map(quoted, columns))
            tables.append(dict(name=name, columns=columns, rowid_alias=alias,
                               **rows_digest(conn.execute('SELECT ' + select + ' FROM ' + quoted(name)))))
        return dict(schema=NATIVE_SCHEMA, schema_rows=rows_digest(schema), tables=tables,
                    skipped_virtual=[], user_version=conn.execute('PRAGMA user_version').fetchone()[0],
                    application_id=conn.execute('PRAGMA application_id').fetchone()[0])


def fixture(root):
    seed = root / 'writer.db'
    program = r'''
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute('PRAGMA journal_mode=WAL')
c.execute('PRAGMA wal_autocheckpoint=0')
c.execute('PRAGMA user_version=19')
c.execute('PRAGMA application_id=562')
c.execute('CREATE TABLE issues(id INTEGER PRIMARY KEY,status TEXT,closed_at TEXT,close_reason TEXT,payload BLOB)')
c.execute("INSERT INTO issues VALUES(1,'open',NULL,'',X'00')")
c.execute('CREATE TABLE labels(issue_id INTEGER,label TEXT)')
c.execute("INSERT INTO labels VALUES(1,'preserve me')")
c.commit()
assert c.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0,0,0)
c.execute('UPDATE issues SET status=?,closed_at=?,close_reason=?,payload=? WHERE id=1',
          ('closed','2026-10-14T00:00:00Z',"WAL-only ' closure λ",b'\x00\xff'*4096))
c.commit()
os._exit(0)
'''
    subprocess.run([sys.executable, '-I', '-S', '-c', program, str(seed)],
                   check=True, capture_output=True, timeout=10)
    source = root / "selected ' λ.db"
    for suffix in ('', '-wal'):
        with component(source, suffix).open('xb') as out:
            out.write(component(seed, suffix).read_bytes())
        component(source, suffix).chmod(0o600)
    oracle = root / 'oracle' / 'beads.db'
    copy_family(source, oracle)
    expected = manifest(oracle)
    return source, expected


def same_manifest(actual, expected):
    # sqlite_schema enumeration order is not an ordering contract.
    left, right = dict(actual), dict(expected)
    left['tables'] = sorted(left['tables'], key=lambda row: row['name'])
    right['tables'] = sorted(right['tables'], key=lambda row: row['name'])
    return left == right


def verify_write_receipt(directory, expected):
    raw_expected = (directory / 'expected.json').read_bytes()
    receipt = json.loads((directory / 'native-write.json').read_text())
    if not (receipt.get('schema') == WRITE_SCHEMA and receipt.get('verified') is True
            and receipt.get('scope') == 'fresh-private-copy'
            and receipt.get('source_checkpoint_performed') is False
            and receipt.get('manifest_sha256') == hashlib.sha256(raw_expected).hexdigest()
            and all(receipt.get(key) is True for key in ('input_family_unchanged',
                    'committed_write_reopened', 'rollback_verified', 'main_only_verified'))):
        raise AssertionError('missing or unbound native write proof')
    row = receipt.get('checkpoint')
    if not (isinstance(row, list) and len(row) == 3 and all(type(x) is int and x == 0 for x in row)):
        raise AssertionError('incomplete native checkpoint')
    relative = receipt.get('sandbox')
    if not (isinstance(relative, str) and relative.startswith('native-write-')
            and Path(relative).name == relative and '/' not in relative and '\\' not in relative):
        raise AssertionError('unsafe native sandbox reference')
    sandbox = directory / relative
    if sandbox.is_symlink():
        raise AssertionError('native sandbox must not be a symlink')
    candidate = sandbox / 'main-only' / 'beads.db'
    data = candidate.read_bytes()
    if (type(receipt.get('main_only_bytes')) is not int or len(data) != receipt['main_only_bytes']
            or hashlib.sha256(data).hexdigest() != receipt.get('main_only_sha256')):
        raise AssertionError('checkpointed image does not match its receipt')
    if json.loads((sandbox / 'complete.json').read_text()) != receipt:
        raise AssertionError('private completion evidence disagrees')
    if not same_manifest(manifest(candidate, immutable=True), expected):
        raise AssertionError('native checkpoint lost or changed original records')
    with contextlib.closing(sqlite3.connect(candidate.as_uri() + '?immutable=1&mode=ro', uri=True)) as conn:
        if conn.execute('SELECT status,closed_at,close_reason,payload FROM issues WHERE id=1').fetchone() != CLOSURE:
            raise AssertionError('native checkpoint lost the WAL-only closure')
        if conn.execute('SELECT name FROM sqlite_schema WHERE name=?', (receipt['probe_table'],)).fetchall():
            raise AssertionError('native probe schema leaked into restored state')
    return receipt


def log_run(root, argv, name):
    result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                            timeout=120, check=False, cwd=root)
    (root / (name + '.stdout')).write_text(result.stdout)
    (root / (name + '.stderr')).write_text(result.stderr)
    return result


class NativeWriteTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-native-write-'))
        self.db, self.expected = fixture(self.root)
        self.before = family(self.db)
        self.output = self.root / 'recovered'

    def run_companion(self, output, *args):
        return log_run(self.root, [str(NATIVE), '--db', str(self.db), '--output', str(output), *args], output.name)

    def test_restore_requires_actual_native_write_and_main_only_proof(self):
        result = self.run_companion(self.output, '--restore-shm')
        self.assertEqual(result.returncode, 0, result.stderr)
        verify_write_receipt(self.output / 'native-rehearsal', self.expected)
        after = family(self.db)
        self.assertEqual(after[''], self.before[''])
        self.assertEqual(after['-wal'], self.before['-wal'])
        self.assertIsNotNone(after['-shm'])

    def test_resume_repeats_native_writes_without_changing_source(self):
        result = self.run_companion(self.output, '--restore-shm')
        self.assertEqual(result.returncode, 0, result.stderr)
        before = family(self.db)
        resumed = self.root / 'resumed'
        result = self.run_companion(resumed, '--resume-from', str(self.output))
        self.assertEqual(result.returncode, 0, result.stderr)
        verify_write_receipt(resumed / 'native-rehearsal', self.expected)
        self.assertEqual(family(self.db), before)

    def direct_probe(self, expected, name):
        db = self.root / name / 'beads.db'
        copy_family(self.db, db)
        manifest(db)  # Initialize only this disposable copy's SHM with SQLite.
        path = db.parent / 'expected.json'
        path.write_text(json.dumps(expected))
        return db, path

    def test_manifest_mismatch_never_reaches_native_writes(self):
        bad = dict(self.expected, user_version=1234567)
        db, path = self.direct_probe(bad, 'bad-manifest')
        before = family(db)
        result = log_run(self.root, [str(NATIVE), '--native-probe', str(path), '--db', str(db)], 'refused')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(db.parent.glob('native-write-*')), [])
        self.assertFalse((db.parent / 'native-write.json').exists())
        self.assertEqual(family(db)[''], before[''])
        self.assertEqual(family(db)['-wal'], before['-wal'])
        self.assertEqual(family(self.db), self.before)

    def test_completion_receipt_collision_is_not_success(self):
        db, path = self.direct_probe(self.expected, 'collision')
        sentinel = db.parent / 'native-write.json'
        sentinel.write_bytes(b'keep prior evidence')
        result = log_run(self.root, [str(NATIVE), '--native-probe', str(path), '--db', str(db)], 'collision')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_bytes(), b'keep prior evidence')
        self.assertEqual(family(self.db), self.before)


class OracleControls(unittest.TestCase):
    """Stock SQLite and deliberately fabricated evidence, NOT native execution."""
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-write-oracle-'))
        self.db, self.expected = fixture(self.root)
        self.directory = self.root / 'observation'
        self.directory.mkdir(mode=0o700)
        self.manifest_path = self.directory / 'expected.json'
        self.manifest_path.write_text(json.dumps(self.expected))

    def modeled_receipt(self, lose_wal=False):
        sandbox = self.directory / 'native-write-fixture'
        candidate = sandbox / 'main-only' / 'beads.db'
        candidate.parent.mkdir(mode=0o700, parents=True)
        if lose_wal:
            candidate.write_bytes(self.db.read_bytes())
        else:
            work = sandbox / 'work' / 'beads.db'
            copy_family(self.db, work)
            with contextlib.closing(sqlite3.connect(work)) as conn:
                conn.execute('PRAGMA wal_autocheckpoint=0')
                conn.execute('CREATE TABLE probe(id INTEGER PRIMARY KEY, note TEXT, payload BLOB)')
                conn.execute("INSERT INTO probe VALUES(1,'committed',?)", (b'\x00\xff'*2048,))
                conn.commit()
                conn.execute("UPDATE probe SET note='aborted'")
                conn.rollback()
                self.assertEqual(conn.execute('SELECT note FROM probe').fetchone(), ('committed',))
                conn.execute('DROP TABLE probe')
                conn.commit()
                self.assertEqual(conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone(), (0,0,0))
                candidate.write_bytes(work.read_bytes())
        data = candidate.read_bytes()
        receipt = dict(schema=WRITE_SCHEMA, verified=True, scope='fresh-private-copy',
                       source_checkpoint_performed=False,
                       manifest_sha256=hashlib.sha256(self.manifest_path.read_bytes()).hexdigest(),
                       sandbox=sandbox.name, probe_table='probe', input_family_unchanged=True,
                       committed_write_reopened=True, rollback_verified=True, checkpoint=[0,0,0],
                       main_only_verified=True, main_only_bytes=len(data),
                       main_only_sha256=hashlib.sha256(data).hexdigest())
        self.save_receipt(receipt)
        return receipt

    def save_receipt(self, receipt):
        (self.directory / 'native-write.json').write_text(json.dumps(receipt))
        (self.directory / 'native-write-fixture' / 'complete.json').write_text(json.dumps(receipt))

    def test_valid_stock_checkpoint_fixture_passes_independent_oracle(self):
        before = family(self.db)
        self.modeled_receipt()
        verify_write_receipt(self.directory, self.expected)
        self.assertEqual(family(self.db), before)

    def test_read_only_success_is_not_write_evidence(self):
        (self.directory / 'native-write.json').write_text(json.dumps(dict(verified=True, schema=NATIVE_SCHEMA)))
        with self.assertRaisesRegex(AssertionError, 'write proof'):
            verify_write_receipt(self.directory, self.expected)

    def test_integrity_ok_and_matching_hash_cannot_hide_lost_wal_commit(self):
        self.modeled_receipt(lose_wal=True)
        with self.assertRaisesRegex(AssertionError, 'lost or changed'):
            verify_write_receipt(self.directory, self.expected)

    def test_incomplete_and_boolean_checkpoint_results_fail(self):
        receipt = self.modeled_receipt()
        for row in ([0,1,1], [1,0,0], [0,-1,-1], [False,0,0], []):
            with self.subTest(row=row):
                self.save_receipt(dict(receipt, checkpoint=row))
                with self.assertRaisesRegex(AssertionError, 'checkpoint'):
                    verify_write_receipt(self.directory, self.expected)

    def test_fresh_manifest_binding_is_required(self):
        receipt = self.modeled_receipt()
        self.save_receipt(dict(receipt, manifest_sha256='0'*64))
        with self.assertRaisesRegex(AssertionError, 'unbound'):
            verify_write_receipt(self.directory, self.expected)

    def test_sandbox_path_cannot_escape_retained_evidence(self):
        receipt = self.modeled_receipt()
        self.save_receipt(dict(receipt, sandbox='native-write-../outside'))
        with self.assertRaisesRegex(AssertionError, 'sandbox'):
            verify_write_receipt(self.directory, self.expected)

    def test_expected_witness_distinguishes_types_and_duplicates(self):
        self.assertNotEqual(rows_digest([(1,)]), rows_digest([(1.0,)]))
        self.assertNotEqual(rows_digest([('a',)]), rows_digest([(b'a',)]))
        self.assertNotEqual(rows_digest([(1,)]), rows_digest([(1,), (1,)]))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--self-test', action='store_true')
    mode.add_argument('--recovery-binary', type=Path)
    args = parser.parse_args()
    if args.recovery_binary:
        original = args.recovery_binary.resolve(strict=True)
        if not original.is_file() or not os.access(original, os.X_OK):
            parser.error('--recovery-binary is not executable')
        install = Path(tempfile.mkdtemp(prefix='br-wal-native-write-install-'))
        NATIVE = install / 'br-wal-recover'
        shutil.copy2(original, NATIVE)
        tests = NativeWriteTests
    else:
        print('ORACLE CONTROLS ONLY: no Rust or native FrankenSQLite is executed.', flush=True)
        tests = OracleControls
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(tests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(0 if result.wasSuccessful() else 1)
