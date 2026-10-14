#!/usr/bin/env python3
"""Independent pager-image recovery checks, using NEW disposable families only.

Default: execute the production Python recovery helper. --recovery-binary
requires an actual compiled companion for separate runtime qualification;
it does not fall back to interpreting the helper. All evidence is retained.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts' / 'recover_orphaned_wal.py'
SPEC = importlib.util.spec_from_file_location('pager_image_recovery', SCRIPT)
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)


def family(db):
    return {suffix: recovery.component(db, suffix).read_bytes()
            if recovery.component(db, suffix).exists() else None
            for suffix in recovery.SUFFIXES}


def file_ids(db):
    return {suffix: recovery.metadata(recovery.component(db, suffix).stat())
            if recovery.component(db, suffix).exists() else None
            for suffix in recovery.SUFFIXES}


def fixture(root, kind='plain', page_size=4096, unfinished=False, utf16=False):
    """Real stock-SQLite commits; copy the abandoned family without its SHM."""
    seed = root / 'seed.db'
    program = r'''
import json, os, sqlite3, sys
path, kind, size, unfinished, utf16 = sys.argv[1:]
c = sqlite3.connect(path)
c.execute('PRAGMA page_size=' + size)
if utf16 == 'yes':
    c.execute("PRAGMA encoding='UTF-16le'")
assert c.execute('PRAGMA journal_mode=WAL').fetchone()[0] == 'wal'
c.execute('PRAGMA wal_autocheckpoint=0')
c.execute('PRAGMA cache_size=5')
c.execute('CREATE TABLE issues(id INTEGER PRIMARY KEY, status TEXT, payload BLOB)')
c.executemany("INSERT INTO issues VALUES (?, 'open', ?)", [(i, b'O' * 8192) for i in range(32)])
if kind in ('fts5', 'external', 'contentless', 'combo'):
    clause = ", content='content', content_rowid='id'" if kind == 'external' else ", content=''" if kind == 'contentless' else ''
    if kind == 'external':
        c.execute('CREATE TABLE content(id INTEGER PRIMARY KEY, body TEXT)')
    c.execute('CREATE VIRTUAL TABLE "search using bogus" USING fts5(body' + clause + ')')
if kind in ('rtree', 'rtree_i32', 'combo'):
    module = 'rtree_i32' if kind == 'rtree_i32' else 'rtree'
    c.execute('CREATE VIRTUAL TABLE bounds USING ' + module + '(id, minx, maxx, miny, maxy)')
if kind == 'shadowed':
    c.execute('CREATE TABLE hidden(rowid TEXT, _rowid_ TEXT, oid TEXT)')
if kind == 'unsupported':
    c.execute('CREATE VIRTUAL TABLE legacy USING fts4(body)')
c.commit()
assert c.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0, 0, 0)
c.execute("UPDATE issues SET status='closed', payload=? WHERE id=0", (b'C' * 8192,))
if kind in ('fts5', 'external', 'contentless', 'combo'):
    if kind == 'external':
        c.execute("INSERT INTO content VALUES (41, 'walonly search sentinel')")
    c.execute('INSERT INTO "search using bogus"(rowid, body) VALUES (41, ?)', ('walonly search sentinel',))
if kind in ('rtree', 'rtree_i32', 'combo'):
    c.execute('INSERT INTO bounds VALUES (41, 2, 8, 3, 9)')
if kind == 'shadowed':
    c.executemany("INSERT INTO hidden VALUES ('same', 'same', 'same')", [(), ()])
if kind == 'unsupported':
    c.execute("INSERT INTO legacy VALUES ('walonly sentinel')")
c.commit()
committed = os.path.getsize(path + '-wal')
if unfinished == 'yes':
    c.execute('BEGIN IMMEDIATE')
    c.execute("UPDATE issues SET status='open', payload=?", (b'U' * 8192,))
    c.execute('CREATE TABLE abandoned(id INTEGER PRIMARY KEY, body BLOB)')
    c.executemany('INSERT INTO abandoned(body) VALUES (?)', [(b'X' * 8192,) for _ in range(32)])
    assert os.path.getsize(path + '-wal') > committed
print(json.dumps({'committed_bytes': committed}), flush=True)
os._exit(0)
'''
    result = subprocess.run([sys.executable, '-I', '-S', '-c', program, str(seed), kind,
                             str(page_size), 'yes' if unfinished else 'no', 'yes' if utf16 else 'no'],
                            capture_output=True, text=True, timeout=20, check=False)
    recovery.require(result.returncode == 0, f'fixture failed: {result.stderr}')
    db = root / "-source ' λ.db"
    for suffix, data in family(seed).items():
        if data is not None and suffix != '-shm':
            with recovery.component(db, suffix).open('xb') as out:
                out.write(data)
    return db


def assert_records(db, kind, *, immutable=True):
    uri = db.as_uri() + ('?mode=ro&immutable=1' if immutable else '?mode=ro')
    with contextlib.closing(sqlite3.connect(uri, uri=True)) as conn:
        recovery.require(conn.execute('PRAGMA integrity_check').fetchall() == [('ok',)], 'bad integrity')
        recovery.require(conn.execute('SELECT status,payload FROM issues WHERE id=0').fetchone()
                         == ('closed', b'C' * 8192), 'WAL-only closure/blob was lost')
        recovery.require(conn.execute("SELECT name FROM sqlite_schema WHERE name='abandoned'").fetchall()
                         == [], 'unfinished schema leaked into recovery')
        if kind in ('fts5', 'external', 'contentless', 'combo'):
            recovery.require(conn.execute('SELECT rowid FROM "search using bogus" WHERE '
                                          '"search using bogus" MATCH ?', ('walonly',)).fetchall() == [(41,)],
                             'committed FTS5 search index was not preserved')
        if kind in ('rtree', 'rtree_i32', 'combo'):
            recovery.require(conn.execute('SELECT id FROM bounds WHERE minx<=5 AND maxx>=5 '
                                          'AND miny<=5 AND maxy>=5').fetchall() == [(41,)],
                             'committed spatial index was not preserved')
        if kind == 'shadowed':
            recovery.require(conn.execute('SELECT count(*) FROM hidden').fetchone() == (2,),
                             'shadowed-rowid records were lost')


class PageImageTests(unittest.TestCase):
    def setUp(self):
        recovery.platform_check()
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-page-test-'))
        self.output = self.root / 'recovered'

    def run_recovery(self, kind='plain', **options):
        db = fixture(self.root, kind, **options)
        before, identities = family(db), file_ids(db)
        report = recovery.recover_copy(db, self.output, copy_last_commit=options.get('unfinished', False))
        self.assertEqual(family(db), before)
        self.assertEqual(file_ids(db), identities)
        candidate = self.output / 'beads.db'
        self.assertEqual(report['page_image']['sha256'], hashlib.sha256(candidate.read_bytes()).hexdigest())
        self.assertEqual(report['page_image']['bytes'], candidate.stat().st_size)
        self.assertEqual(report, json.loads((self.output / 'complete.json').read_text()))
        assert_records(candidate, kind)
        return report

    def test_complete_wal_exact_image_is_independently_attested(self):
        self.run_recovery()

    def test_fts5_index_and_search_survive_wal_recovery(self):
        report = self.run_recovery('fts5')
        self.assertEqual(report['logical']['page_verified_tables'][0]['module'], 'fts5')
        self.assertIn('search using bogus_data', [table['name'] for table in report['logical']['tables']])

    def test_external_content_fts5_keeps_content_and_search_index(self):
        self.run_recovery('external')

    def test_contentless_fts5_keeps_its_index(self):
        self.run_recovery('contentless')

    def test_rtree_keeps_spatial_queries(self):
        self.run_recovery('rtree')

    def test_integer_rtree_keeps_spatial_queries(self):
        self.run_recovery('rtree_i32')

    def test_shadowed_hidden_rowids_are_bound_by_the_complete_image(self):
        report = self.run_recovery('shadowed')
        self.assertIn({'name': 'hidden', 'kind': 'shadowed-rowid'}, report['logical']['page_verified_tables'])

    def test_512_byte_pages_and_utf16_keep_exact_image(self):
        self.run_recovery('combo', page_size=512, utf16=True)

    def test_65536_byte_pages_keep_exact_image(self):
        self.run_recovery('fts5', page_size=65536)

    def test_real_unfinished_tail_preserves_committed_search_state(self):
        report = self.run_recovery('fts5', unfinished=True)
        self.assertGreater(report['wal']['excluded_tail']['bytes'], 0)
        self.assertTrue(report['copy_last_commit_requested'])

    def test_restoration_and_resume_preserve_existing_pages_and_index(self):
        db = fixture(self.root, 'combo')
        before = family(db)
        report = recovery.recover_copy(db, self.output, restore_shm=True)
        after, identities = family(db), file_ids(db)
        for suffix in before:
            if suffix != '-shm':
                self.assertEqual(before[suffix], after[suffix])
        self.assertEqual(report['page_image']['sha256'], report['logical']['page_image']['sha256'])
        resumed = recovery.recover_copy(db, self.root / 'resumed', resume_from=self.output)
        self.assertTrue(resumed['source_unchanged'])
        self.assertEqual(family(db), after)
        self.assertEqual(file_ids(db), identities)
        assert_records(db, 'combo', immutable=False)
        with contextlib.closing(sqlite3.connect(db)) as conn:
            conn.execute('INSERT INTO "search using bogus"(rowid,body) VALUES (42, ?)', ('later index write',))
            conn.commit()
            self.assertEqual(conn.execute('SELECT rowid FROM "search using bogus" WHERE '
                                          '"search using bogus" MATCH ?', ('walonly',)).fetchall(), [(41,)])

    def assert_replay_damage_refused(self, damage):
        db = fixture(self.root)
        before = family(db)
        original = recovery.replay
        def damaged(main, wal, plan, candidate):
            original(main, wal, plan, candidate)
            logical_before = recovery.logical_witness(candidate, immutable=True)
            damage(candidate)
            # A real discriminator: row/integrity witnesses accept the change.
            self.assertEqual(recovery.logical_witness(candidate, immutable=True), logical_before)
        with mock.patch.object(recovery, 'replay', side_effect=damaged):
            with self.assertRaisesRegex(recovery.RecoveryError, 'page image differs'):
                recovery.recover_copy(db, self.output)
        self.assertEqual(family(db), before)
        self.assertFalse(self.output.exists())

    def test_equal_rows_do_not_hide_changed_reserved_header_bytes(self):
        def damage(candidate):
            with candidate.open('r+b') as out:
                out.seek(72)
                out.write(b'\x01')
        self.assert_replay_damage_refused(damage)

    def test_equal_rows_do_not_hide_added_physical_pages(self):
        def damage(candidate):
            with candidate.open('ab') as out:
                out.write(b'\0' * 4096)
        self.assert_replay_damage_refused(damage)

    def test_header_counters_are_not_normalized_away(self):
        def damage(candidate):
            with candidate.open('r+b') as out:
                for offset in (24, 92):
                    out.seek(offset)
                    out.write((999).to_bytes(4, 'big'))
        self.assert_replay_damage_refused(damage)

    def test_serializer_error_cannot_downgrade_to_rows_only(self):
        db = fixture(self.root)
        with mock.patch.object(recovery, 'serialized_image_witness',
                               side_effect=sqlite3.DatabaseError('injected serializer failure')):
            with self.assertRaisesRegex(recovery.RecoveryError, 'serializer failure'):
                recovery.recover_copy(db, self.output)
        self.assertFalse(self.output.exists())

    def test_allocation_bound_is_checked_before_serialization(self):
        class BoundedConnection:
            def execute(self, sql):
                return mock.Mock(fetchone=lambda: (4096 if 'page_size' in sql else 4,))
            def serialize(self, **kwargs):
                raise AssertionError('oversized serialization was attempted')
        with self.assertRaisesRegex(recovery.RecoveryError, 'exceeds --max-bytes'):
            recovery.serialized_image_witness(BoundedConnection(), 4096)

    def test_missing_serializer_keeps_ordinary_legacy_contract_only(self):
        db = fixture(self.root)
        real_connect = sqlite3.connect
        class LegacyConnection(sqlite3.Connection):
            serialize = None
        def legacy(*args, **kwargs):
            return real_connect(*args, factory=LegacyConnection, **kwargs)
        with mock.patch.object(sqlite3, 'connect', side_effect=legacy):
            report = recovery.recover_copy(db, self.output)
        self.assertNotIn('page_image', report)
        assert_records(self.output / 'beads.db', 'plain')

    def test_missing_serializer_refuses_virtual_tables(self):
        db = fixture(self.root, 'fts5')
        real_connect = sqlite3.connect
        class LegacyConnection(sqlite3.Connection):
            serialize = None
        def legacy(*args, **kwargs):
            return real_connect(*args, factory=LegacyConnection, **kwargs)
        with mock.patch.object(sqlite3, 'connect', side_effect=legacy):
            with self.assertRaisesRegex(recovery.RecoveryError, 'serialization support'):
                recovery.recover_copy(db, self.output)
        self.assertFalse(self.output.exists())

    def test_unreviewed_virtual_module_remains_refused(self):
        db = fixture(self.root, 'unsupported')
        with self.assertRaisesRegex(recovery.RecoveryError, 'engine-specific verification'):
            recovery.recover_copy(db, self.output)
        self.assertFalse(self.output.exists())

    def test_oracle_cannot_rewrite_protected_pages_in_strict_mode(self):
        db = fixture(self.root)
        original = recovery.logical_witness
        def changed(path, **kwargs):
            witness = original(path, **kwargs)
            if path.parent.name == 'oracle':
                with path.open('r+b') as out:
                    out.seek(72)
                    out.write(b'\x01')
            return witness
        with mock.patch.object(recovery, 'logical_witness', side_effect=changed):
            with self.assertRaisesRegex(recovery.RecoveryError, 'oracle changed protected bytes'):
                recovery.recover_copy(db, self.output)
        self.assertFalse(self.output.exists())

    def test_module_recognition_does_not_trust_words_inside_identifiers(self):
        accepted = ['CREATE VIRTUAL TABLE "using bogus" USING fts5(body)',
                    'CREATE /* comment */ VIRTUAL TABLE main.[x] USING rtree(id,x,y)',
                    'CREATE VIRTUAL TABLE IF NOT EXISTS x USING "rtree_i32"(id,x,y)']
        for sql, module in zip(accepted, ('fts5', 'rtree', 'rtree_i32')):
            self.assertEqual(recovery.builtin_virtual_module(sql), module)
        for sql in ['CREATE VIRTUAL TABLE "USING fts5" USING other(body)',
                    'CREATE VIRTUAL TABLE x USING fts4(body)',
                    'CREATE TABLE x(body TEXT)',
                    'CREATE VIRTUAL TABLE x USING fts5evil(body)']:
            self.assertIsNone(recovery.builtin_virtual_module(sql))


def qualify_native(binary):
    binary = binary.resolve(strict=True)
    recovery.require(binary.is_file() and os.access(binary, os.X_OK), 'compiled recovery executable required')
    root = Path(tempfile.mkdtemp(prefix='br-wal-page-native-'))
    for kind in ('fts5', 'rtree', 'shadowed'):
        case = root / kind
        case.mkdir()
        db = fixture(case, kind)
        before, ids = family(db), file_ids(db)
        output = case / 'recovered'
        result = subprocess.run([str(binary), '--db', str(db), '--output', str(output)],
                                capture_output=True, text=True, timeout=60, check=False)
        (case / 'stdout.log').write_text(result.stdout)
        (case / 'stderr.log').write_text(result.stderr)
        recovery.require(result.returncode == 0, f'compiled recovery failed: {result.stderr}')
        report = json.loads(result.stdout)
        image = output / 'beads.db'
        recovery.require(report['page_image']['sha256'] == hashlib.sha256(image.read_bytes()).hexdigest(),
                         'compiled recovery lacks exact image evidence')
        recovery.require(family(db) == before and file_ids(db) == ids, 'compiled recovery changed source')
        assert_records(image, kind)
    print(f'Compiled companion: FTS5, RTree and hidden-rowid recovery passed. Evidence: {root}')


if __name__ == '__main__':
    if '--recovery-binary' in sys.argv[1:]:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument('--recovery-binary', type=Path, required=True)
        qualify_native(parser.parse_args().recovery_binary)
    else:
        unittest.main(verbosity=2)
