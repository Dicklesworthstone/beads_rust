#!/usr/bin/env python3
"""GH #562: revalidate interrupted SHM installs without replacing live files.

Run directly for production Python-helper tests. No br executable is simulated.
All databases, interruptions and corruptions are confined to new disposable
copies. Retain every fixture, including failed evidence, for inspection.
"""
from __future__ import annotations

import contextlib
import importlib.util
import json
import os
from pathlib import Path
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts' / 'recover_orphaned_wal.py'
SPEC = importlib.util.spec_from_file_location('wal_resume_recovery', SCRIPT)
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)
STAMP = '2026-10-14T00:00:00Z'
REASON = 'committed only in WAL before recovery interruption'
EXPECTED = [('closed', STAMP, REASON, b'\x00\xff' * 4096)]
QUERY = 'SELECT status, closed_at, close_reason, payload FROM issues'


def family(db):
    return {suffix: (recovery.component(db, suffix).read_bytes()
                     if recovery.component(db, suffix).exists() else None)
            for suffix in recovery.SUFFIXES}


def identity(db):
    return {suffix: (recovery.metadata(recovery.component(db, suffix).stat())
                     if recovery.component(db, suffix).exists() else None)
            for suffix in recovery.SUFFIXES}


def evidence_bytes(path):
    return {str(file.relative_to(path)): file.read_bytes()
            for file in path.rglob('*') if file.is_file()}


def make_orphan(root):
    seed = root / 'seed.db'
    program = r'''
import os, sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
conn.execute('PRAGMA journal_mode=WAL')
conn.execute('PRAGMA wal_autocheckpoint=0')
conn.execute('CREATE TABLE issues (id TEXT PRIMARY KEY, status TEXT, closed_at TEXT, close_reason TEXT, payload BLOB)')
conn.execute("INSERT INTO issues VALUES ('w562-1', 'open', NULL, '', X'00')")
conn.commit()
assert conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0, 0, 0)
conn.execute("UPDATE issues SET status='closed', closed_at=?, close_reason=?, payload=?",
             (sys.argv[2], sys.argv[3], b'\x00\xff' * 4096))
conn.commit()
os._exit(0)
'''
    subprocess.run([sys.executable, '-I', '-S', '-c', program, str(seed), STAMP, REASON],
                   check=True, timeout=10, capture_output=True)
    source = root / "-source ' $HOME λ.db"
    for suffix, data in family(seed).items():
        if suffix != '-shm' and data is not None:
            with recovery.component(source, suffix).open('xb') as file:
                file.write(data)
    return source


def interrupt_restore(db, evidence, boundary='after-install'):
    program = r'''
import importlib.util, os, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('recovery', sys.argv[1])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
db, evidence, boundary = Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4]
rename, write = m.publish_no_replace, m.write_json
def crash_rename(source, destination):
    installing = destination == m.component(db, '-shm')
    if installing and boundary == 'before-install':
        os._exit(73)
    rename(source, destination)
    if installing and boundary == 'after-install':
        os._exit(73)
def crash_complete(path, value):
    if path == evidence / 'complete.json' and boundary == 'before-complete':
        os._exit(73)
    write(path, value)
m.publish_no_replace, m.write_json = crash_rename, crash_complete
m.recover_copy(db, evidence, restore_shm=True)
raise SystemExit(99)
'''
    result = subprocess.run([sys.executable, '-I', '-S', '-c', program, str(SCRIPT),
                             str(db), str(evidence), boundary], capture_output=True,
                            text=True, timeout=20, check=False)
    if result.returncode != 73:
        raise AssertionError(f'fixture did not reach {boundary}: {result.returncode}: {result.stderr}')


@contextlib.contextmanager
def reader(db):
    program = r'''
import os, sqlite3, sys
conn = sqlite3.connect(sys.argv[1], timeout=0)
conn.execute('BEGIN')
conn.execute('SELECT * FROM issues').fetchall()
print('ready', flush=True)
sys.stdin.readline()
os._exit(0)
'''
    child = subprocess.Popen([sys.executable, '-I', '-S', '-u', '-c', program, str(db)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    messages = queue.Queue()
    observer = threading.Thread(target=lambda: messages.put(child.stdout.readline()), daemon=True)
    observer.start()
    try:
        if messages.get(timeout=10).strip() != 'ready':
            raise AssertionError('stock reader did not obtain its snapshot')
        yield
    finally:
        try:
            child.communicate('\n', timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate(timeout=5)
        observer.join(timeout=5)


class ResumeTests(unittest.TestCase):
    def setUp(self):
        recovery.platform_check()
        self.root = Path(tempfile.mkdtemp(prefix='br-wal-resume-test-'))
        self.db = make_orphan(self.root)
        self.evidence = self.root / 'interrupted'
        self.output = self.root / 'resumed'

    def crashed(self, boundary='after-install'):
        interrupt_restore(self.db, self.evidence, boundary)
        self.assertFalse((self.evidence / 'complete.json').exists())

    def assert_refused(self, pattern):
        before, ids = family(self.db), identity(self.db)
        evidence = evidence_bytes(self.evidence)
        with self.assertRaisesRegex(recovery.RecoveryError, pattern):
            recovery.recover_copy(self.db, self.output, resume_from=self.evidence)
        self.assertEqual(family(self.db), before)
        self.assertEqual(identity(self.db), ids)
        self.assertEqual(evidence_bytes(self.evidence), evidence)
        self.assertFalse((self.output / 'complete.json').exists())

    def assert_resumed(self):
        before, ids = family(self.db), identity(self.db)
        evidence = evidence_bytes(self.evidence)
        real_query = recovery.logical_witness
        def private_query(path, **kwargs):
            self.assertNotEqual(path, self.db, 'resume opened the source through SQLite')
            return real_query(path, **kwargs)
        with mock.patch.object(recovery, 'logical_witness', side_effect=private_query), \
                mock.patch.object(recovery, 'restore_missing_index',
                                  side_effect=AssertionError('resume must not reinstall SHM')):
            report = recovery.recover_copy(self.db, self.output, resume_from=self.evidence)
        self.assertEqual(report['database_path'], str(self.db))
        self.assertTrue(report['source_unchanged'])
        self.assertFalse(report['live_recovery'])
        self.assertFalse(report['checkpoint_performed'])
        self.assertEqual(report['resume']['schema'], 'br.orphaned_wal.resume.v1')
        self.assertEqual(report['resume']['evidence_path'], str(self.evidence))
        self.assertTrue(report['resume']['existing_index_revalidated'])
        self.assertEqual(json.loads((self.output / 'complete.json').read_text()), report)
        self.assertEqual(family(self.db), before)
        self.assertEqual(identity(self.db), ids)
        self.assertEqual(evidence_bytes(self.evidence), evidence)
        recovered = Path(report['recovered_copy_path'])
        with contextlib.closing(sqlite3.connect(recovered.as_uri() + '?immutable=1', uri=True)) as conn:
            self.assertEqual(conn.execute(QUERY).fetchall(), EXPECTED)
        return report

    def test_process_death_after_index_rename_can_complete_without_reinstall(self):
        self.crashed()
        self.assertFalse((self.evidence / 'shm-install-complete.json').exists())
        self.assert_resumed()

    def test_process_death_before_final_receipt_can_complete_without_reinstall(self):
        self.crashed('before-complete')
        self.assertTrue((self.evidence / 'shm-install-complete.json').exists())
        self.assert_resumed()

    def test_already_completed_restore_is_revalidated_idempotently(self):
        recovery.recover_copy(self.db, self.evidence, restore_shm=True)
        self.assert_resumed()
        self.output = self.root / 'resumed-again'
        self.assert_resumed()

    def test_resume_refuses_before_install_and_original_restore_can_retry(self):
        self.crashed('before-install')
        self.assert_refused('installed SHM is missing')
        report = recovery.recover_copy(self.db, self.root / 'retry', restore_shm=True)
        self.assertTrue(report['live_recovery'])

    def test_plain_copy_evidence_never_authorizes_resume(self):
        recovery.recover_copy(self.db, self.evidence)
        self.assert_refused('restoration evidence|installed SHM is missing')

    def test_changed_wal_fails_closed(self):
        self.crashed()
        wal = recovery.component(self.db, '-wal')
        with wal.open('r+b') as file:
            file.seek(60)
            file.write(b'changed')
        self.assert_refused('source.*changed|source.*match')

    def test_same_bytes_on_new_main_inode_fail_closed(self):
        self.crashed()
        original = self.root / 'original-inode'
        self.db.rename(original)
        self.db.write_bytes(original.read_bytes())
        self.assert_refused('source.*changed|source.*match')

    def test_modified_shm_fails_closed(self):
        self.crashed()
        shm = recovery.component(self.db, '-shm')
        with shm.open('r+b') as file:
            file.seek(200)
            file.write(b'wrong page index')
        self.assert_refused('installed SHM.*match|index.*match')

    def test_corrupt_archived_payload_fails_closed(self):
        self.crashed()
        (self.evidence / 'before' / 'beads.db-wal').write_bytes(b'damaged archive')
        self.assert_refused('evidence.*changed|evidence.*match')

    def test_active_stock_reader_refuses_without_source_mutation(self):
        self.crashed()
        with reader(self.db):
            self.assert_refused('busy')

    def test_ambiguous_json_keys_are_not_evidence(self):
        self.crashed()
        prepared = self.evidence / 'prepared.json'
        text = prepared.read_text()
        prepared.write_text(text.replace('"schema":', '"schema":"wrong","schema":', 1))
        self.assert_refused('duplicate')

    def test_boolean_metadata_cannot_alias_an_integer(self):
        self.crashed()
        path = self.evidence / 'prepared.json'
        doc = json.loads(path.read_text())
        doc['raw']['']['uid'] = bool(doc['raw']['']['uid'])
        path.write_text(json.dumps(doc))
        self.assert_refused('source.*match|source.*changed|evidence.*match')

    def test_existing_destination_is_not_overwritten(self):
        self.crashed()
        self.output.mkdir()
        (self.output / 'sentinel').write_bytes(b'keep')
        self.assert_refused('destination already exists')
        self.assertEqual((self.output / 'sentinel').read_bytes(), b'keep')

    def test_evidence_symlink_is_not_followed(self):
        self.crashed()
        link = self.root / 'evidence-link'
        link.symlink_to(self.evidence, target_is_directory=True)
        with self.assertRaises((OSError, recovery.RecoveryError)):
            recovery.recover_copy(self.db, self.output, resume_from=link)
        self.assertFalse(self.output.exists())

    def test_cli_resume_uses_literal_paths_and_emits_completion(self):
        self.crashed()
        result = subprocess.run([sys.executable, '-I', '-S', str(SCRIPT), '--db', str(self.db),
                                 '--output', str(self.output), '--resume-from', str(self.evidence)],
                                capture_output=True, text=True, check=False, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)['resume']['existing_index_revalidated'])
        self.assertFalse((self.evidence / 'complete.json').exists())

    def test_restore_and_resume_modes_conflict_before_opening_source(self):
        with self.assertRaisesRegex(recovery.RecoveryError, 'mutually exclusive'):
            recovery.recover_copy(self.root / 'absent.db', self.output, restore_shm=True,
                                  resume_from=self.evidence)
        self.assertFalse(self.output.exists())


if __name__ == '__main__':
    unittest.main(verbosity=2)
