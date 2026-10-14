#!/usr/bin/env python3
"""Committed-freelist recovery on NEW disposable families only.

--self-test checks real stock-SQLite fixtures and independent oracles; it does
NOT run Rust. --recovery-binary requires an actual compiled br-wal-recover and
never falls back to Python or a fake responder. All evidence is retained.
"""
from __future__ import annotations

import argparse
import contextlib
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
import unittest

NATIVE = None
SCHEMA = 'br.orphaned_wal.native_probe.v1'
SUFFIXES = ('', '-wal', '-shm', '-journal', '-wal-cert', '-wal-cert-head',
            '-ns', '-fsqlite-ns-gate', '-fsqlite-ns-use', '.fsqlite-migration-state')
CLOSURE = ('closed', '2026-10-14T00:00:00Z', 'freelist WAL-only closure', b'\x00\xff' * 4096)
QUERY = 'SELECT status,closed_at,close_reason,payload FROM issues WHERE id=1'


def require(value, message):
    if not value:
        raise AssertionError(message)


def component(db, suffix):
    return db.with_name(db.name + suffix)


def family(db):
    result = {}
    for suffix in SUFFIXES:
        path = component(db, suffix)
        if path.exists():
            meta = path.stat()
            result[suffix] = (path.read_bytes(), meta.st_dev, meta.st_ino,
                              stat.S_IMODE(meta.st_mode), meta.st_mtime_ns, meta.st_ctime_ns)
        else:
            result[suffix] = None
    return result


def copy_family(db, output):
    output.parent.mkdir(mode=0o700, parents=True, exist_ok=False)
    for suffix in ('', '-wal'):
        source = component(db, suffix)
        if source.exists():
            with component(output, suffix).open('xb') as out:
                out.write(source.read_bytes())
            component(output, suffix).chmod(0o600)
    return output


@contextlib.contextmanager
def connection(db, immutable=False):
    uri = db.as_uri() + ('?mode=ro&immutable=1' if immutable else '?mode=ro')
    with contextlib.closing(sqlite3.connect(uri, uri=True, timeout=0)) as conn:
        conn.execute('PRAGMA trusted_schema=OFF')
        conn.execute('PRAGMA query_only=ON')
        yield conn


def rows_digest(rows):
    encoded = []
    for row in rows:
        data = bytearray(struct.pack('>Q', len(row)))
        for item in row:
            if item is None:
                data += b'n'
            elif type(item) is int:
                data += b'i' + struct.pack('>q', item)
            elif type(item) is float:
                data += b'r' + struct.pack('>d', item)
            else:
                raw = item if isinstance(item, bytes) else item.encode('utf-8')
                data += (b'b' if isinstance(item, bytes) else b't') + struct.pack('>Q', len(raw)) + raw
        encoded.append(bytes(data))
    digest = hashlib.sha256()
    for data in sorted(encoded):
        digest.update(struct.pack('>Q', len(data)))
        digest.update(data)
    return dict(rows=len(encoded), sha256=digest.hexdigest())


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def manifest(db, immutable=False):
    with connection(db, immutable) as conn:
        require(conn.execute('PRAGMA integrity_check').fetchall() == [('ok',)], 'bad SQLite integrity')
        conn.execute('BEGIN')
        schema = conn.execute('SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema').fetchall()
        tables = []
        for kind, name, _, _, _ in schema:
            if kind != 'table':
                continue
            columns = [row[1] for row in conn.execute('PRAGMA table_xinfo(' + quote(name) + ')')]
            alias = next(a for a in ('rowid', '_rowid_', 'oid') if a not in {c.lower() for c in columns})
            projection = alias + ',' + ','.join(map(quote, columns))
            tables.append(dict(name=name, columns=columns, rowid_alias=alias,
                               **rows_digest(conn.execute('SELECT ' + projection + ' FROM ' + quote(name)))))
        return dict(schema=SCHEMA, schema_rows=rows_digest(schema),
                    tables=sorted(tables, key=lambda row: row['name']), skipped_virtual=[],
                    user_version=conn.execute('PRAGMA user_version').fetchone()[0],
                    application_id=conn.execute('PRAGMA application_id').fetchone()[0])


def normalized(value):
    return dict(value, tables=sorted(value['tables'], key=lambda row: row['name']))


def fixture(root):
    seed = root / 'writer.db'
    program = r'''
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute('PRAGMA page_size=512')
c.execute('PRAGMA auto_vacuum=NONE')
assert c.execute('PRAGMA journal_mode=WAL').fetchone() == ('wal',)
c.execute('PRAGMA wal_autocheckpoint=0')
c.execute('PRAGMA user_version=19')
c.execute('PRAGMA application_id=562')
c.execute('CREATE TABLE issues(id INTEGER PRIMARY KEY,status TEXT,closed_at TEXT,close_reason TEXT,payload BLOB)')
c.execute("INSERT INTO issues VALUES(1,'open',NULL,'',X'00')")
c.execute('CREATE TABLE discarded(payload BLOB)')
c.executemany('INSERT INTO discarded VALUES (?)', [(b'B' * 8192,) for _ in range(24)])
c.commit()
c.execute('DROP TABLE discarded')
c.commit()
assert c.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0,0,0)
assert c.execute('PRAGMA freelist_count').fetchone()[0] > 0
c.execute('UPDATE issues SET status=?,closed_at=?,close_reason=?,payload=? WHERE id=1',
          ('closed','2026-10-14T00:00:00Z','freelist WAL-only closure',b'\x00\xff'*4096))
c.commit()
assert c.execute('PRAGMA freelist_count').fetchone()[0] > 0
os._exit(0)
'''
    subprocess.run([sys.executable, '-I', '-S', '-c', program, str(seed)],
                   check=True, capture_output=True, timeout=15)
    source = copy_family(seed, root / 'source' / "-freelist ' λ.db")
    require(not component(source, '-shm').exists(), 'fixture must omit SHM only on its copy')
    require(component(source, '-wal').stat().st_size > 32, 'fixture needs committed WAL frames')
    oracle = copy_family(source, root / 'oracle' / 'beads.db')
    expected = manifest(oracle)
    with connection(oracle) as conn:
        require(conn.execute(QUERY).fetchone() == CLOSURE, 'WAL-only fixture did not commit')
        require(conn.execute('PRAGMA freelist_count').fetchone()[0] > 0, 'fixture lost its free pages')
    return source, expected


def checkpoint_copy(source, directory):
    db = copy_family(source, directory / 'beads.db')
    with contextlib.closing(sqlite3.connect(db)) as conn:
        require(conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0, 0, 0),
                'stock checkpoint did not finish')
    return db


def corrupt_freelist(db):
    # Only a newly checkpointed disposable file. Point its freelist trunk past
    # EOF without modifying any table page, row or recorded freelist count.
    raw = bytearray(db.read_bytes())
    require(int.from_bytes(raw[36:40], 'big') > 0, 'negative control needs a freelist')
    pages = int.from_bytes(raw[28:32], 'big')
    raw[32:36] = (pages + 1).to_bytes(4, 'big')
    db.write_bytes(raw)


def validate_native_output(output, expected):
    directory = output / 'native-rehearsal'
    raw_manifest = (directory / 'expected.json').read_bytes()
    require(normalized(json.loads(raw_manifest)) == expected, 'native expectation changed')
    receipt = json.loads((directory / 'native-write.json').read_text())
    require(receipt.get('schema') == 'br.orphaned_wal.native_write.v1'
            and receipt.get('verified') is True and receipt.get('scope') == 'fresh-private-copy'
            and receipt.get('source_checkpoint_performed') is False
            and receipt.get('manifest_sha256') == hashlib.sha256(raw_manifest).hexdigest()
            and all(receipt.get(key) is True for key in ('committed_write_reopened',
                    'rollback_verified', 'main_only_verified', 'input_family_unchanged')),
            'missing native write/checkpoint proof')
    require(receipt.get('checkpoint') == [0, 0, 0]
            and all(type(item) is int for item in receipt['checkpoint']), 'invalid native checkpoint')
    name = receipt.get('sandbox')
    require(isinstance(name, str) and re.fullmatch(r'native-write-[A-Za-z0-9_-]+', name),
            'unsafe sandbox path')
    sandbox = directory / name
    require(not sandbox.is_symlink() and not (sandbox / 'main-only').is_symlink(), 'unsafe sandbox')
    require(json.loads((sandbox / 'complete.json').read_text()) == receipt, 'incomplete write evidence')
    candidate = sandbox / 'main-only' / 'beads.db'
    data = candidate.read_bytes()
    require(type(receipt.get('main_only_bytes')) is int and len(data) == receipt['main_only_bytes']
            and hashlib.sha256(data).hexdigest() == receipt.get('main_only_sha256'), 'wrong main image')
    require(manifest(candidate, immutable=True) == expected, 'native checkpoint lost original data')
    with connection(candidate, immutable=True) as conn:
        require(conn.execute(QUERY).fetchone() == CLOSURE, 'native checkpoint lost the WAL-only commit')
        require(conn.execute('PRAGMA freelist_count').fetchone()[0] > 0,
                'native case no longer exercises a committed freelist')


class FixtureTests(unittest.TestCase):
    """Stock SQLite only: these controls do not qualify the native verifier."""
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-freelist-oracle-'))
        self.db, self.expected = fixture(self.root)

    def test_freelist_and_wal_only_commit_are_both_real(self):
        before = family(self.db)
        with connection(self.db, immutable=True) as conn:
            self.assertEqual(conn.execute('PRAGMA integrity_check').fetchall(), [('ok',)])
            self.assertGreater(conn.execute('PRAGMA freelist_count').fetchone()[0], 0)
            self.assertEqual(conn.execute(QUERY).fetchone()[0], 'open')
        self.assertEqual(family(self.db), before)

    def test_private_wal_reader_preserves_complete_manifest_and_source(self):
        before = family(self.db)
        copy = copy_family(self.db, self.root / 'private' / 'beads.db')
        self.assertEqual(manifest(copy), self.expected)
        self.assertEqual(family(self.db), before)

    def test_checkpointed_main_keeps_commit_and_committed_free_pages(self):
        db = checkpoint_copy(self.db, self.root / 'checkpoint')
        self.assertEqual(manifest(db, immutable=True), self.expected)
        with connection(db, immutable=True) as conn:
            self.assertGreater(conn.execute('PRAGMA freelist_count').fetchone()[0], 0)
            self.assertEqual(conn.execute(QUERY).fetchone(), CLOSURE)

    def test_corrupt_freelist_is_detected_even_when_table_rows_are_intact(self):
        db = checkpoint_copy(self.db, self.root / 'damaged')
        corrupt_freelist(db)
        with connection(db, immutable=True) as conn:
            self.assertEqual(conn.execute(QUERY).fetchone(), CLOSURE)
            self.assertNotEqual(conn.execute('PRAGMA integrity_check').fetchall(), [('ok',)])
        with self.assertRaisesRegex(AssertionError, 'integrity'):
            manifest(db, immutable=True)

    def test_integrity_ok_main_without_wal_does_not_match_committed_manifest(self):
        self.assertNotEqual(manifest(self.db, immutable=True), self.expected)


class NativeTests(unittest.TestCase):
    """Actual compiled companion only; no payload or receipt simulation."""
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-freelist-native-'))
        self.db, self.expected = fixture(self.root)

    def run_native(self, args, name):
        result = subprocess.run([str(NATIVE), *map(str, args)], cwd=self.root,
                                capture_output=True, text=True, timeout=180, check=False)
        (self.root / (name + '.stdout')).write_text(result.stdout)
        (self.root / (name + '.stderr')).write_text(result.stderr)
        return result

    def restore(self):
        output = self.root / 'restored'
        before = family(self.db)
        result = self.run_native(['--db', self.db, '--output', output, '--restore-shm'], 'restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertIs(report['native_probe']['verified'], True)
        validate_native_output(output, self.expected)
        after = family(self.db)
        self.assertIsNotNone(after['-shm'])
        self.assertEqual({**after, '-shm': None}, before)
        return output

    def test_restore_qualifies_writes_with_nonempty_committed_freelist(self):
        self.restore()

    def test_resume_repeats_native_write_proof_without_changing_source(self):
        prior = self.restore()
        before = family(self.db)
        output = self.root / 'resumed'
        result = self.run_native(['--db', self.db, '--output', output, '--resume-from', prior], 'resume')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIs(json.loads(result.stdout)['resume']['existing_index_revalidated'], True)
        validate_native_output(output, self.expected)
        self.assertEqual(family(self.db), before)

    def test_actual_freelist_damage_is_not_waived_by_native_verification(self):
        db = checkpoint_copy(self.db, self.root / 'damaged')
        expected = manifest(db, immutable=True)
        receipt = db.parent / 'expected.json'
        receipt.write_text(json.dumps(expected))
        corrupt_freelist(db)
        before = db.read_bytes()
        result = self.run_native(['--native-probe', receipt, '--db', db], 'corrupt-probe')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('integrity', result.stderr.lower())
        self.assertFalse((db.parent / 'native-write.json').exists())
        self.assertEqual(db.read_bytes(), before)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--self-test', action='store_true')
    mode.add_argument('--recovery-binary', type=Path)
    args = parser.parse_args()
    if args.recovery_binary:
        NATIVE = args.recovery_binary.resolve(strict=True)
        require(NATIVE.is_file() and os.access(NATIVE, os.X_OK), 'native executable is required')
    print(f'Python {sys.version.split()[0]}, SQLite {sqlite3.sqlite_version}; '
          + ('ACTUAL NATIVE tests' if NATIVE else 'stock-SQLite fixture controls ONLY'), flush=True)
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(NativeTests if NATIVE else FixtureTests)
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
