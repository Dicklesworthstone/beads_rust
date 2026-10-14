#!/usr/bin/env python3
"""Pre-install native-gate controls. Default tests use an explicitly FAKE responder.

They prove refusal/ordering/preservation and the independent witness protocol,
NOT FrankenSQLite correctness. --recovery-binary instead requires an actual
compiled companion and never substitutes the responder. Disposable inputs only.
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
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from test_wal_recovery_resume import make_orphan, family, identity, evidence_bytes, EXPECTED, QUERY
from test_wal_recovery_resume import recovery, interrupt_restore


# This responder is NOT FrankenSQLite. Stock SQLite on disposable copies models
# native write artifacts so the production parent's independent checks run too.
# It must never count as native qualification, even for the positive control.
RESPONDER = r'''
import contextlib, fcntl, hashlib, json, os, shutil, sqlite3, struct, sys, time
from pathlib import Path
manifest = Path(sys.argv[2]); db = Path(sys.argv[4])
assert sys.argv[1] == '--native-probe'
assert db.parent.name == 'native-rehearsal'
assert not any(k.startswith(('BEADS_', 'BD_', 'BR_', 'FSQLITE_')) for k in os.environ)
mode = Path(__file__).with_suffix('.mode').read_text()
for offset, length in [(120, 3), (128, 1)]:
    with open(str(db)+'-shm', 'r+b') as f:
        try:
            fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, length, offset)
        except BlockingIOError:
            pass
        else:
            raise AssertionError('private recovery/initialization lock was not retained')
reply = dict(schema='br.orphaned_wal.native_probe.v1', verified=True,
             engine='frankensqlite', engine_version='TEST-RESPONDER-NOT-AN-ENGINE',
             manifest_sha256=hashlib.sha256(manifest.read_bytes()).hexdigest())
if mode == 'exit':
    sys.exit(7)
if mode == 'timeout':
    time.sleep(5)
if mode == 'garbage':
    print('not json'); sys.exit(0)
if mode == 'duplicate':
    print(json.dumps(reply)[:-1]+',"verified":true}'); sys.exit(0)
if mode == 'nonfinite':
    print(json.dumps(reply)[:-1]+',"extra":NaN}'); sys.exit(0)
if mode == 'wrong-manifest':
    reply['manifest_sha256'] = '0' * 64
if mode == 'unverified':
    reply['verified'] = 1
if mode == 'unknown-engine':
    reply['engine_version'] = 'unknown'
if mode == 'wrong-engine':
    reply['engine'] = 'sqlite'
if mode == 'extra':
    reply['extra'] = 'undeclared'
if mode == 'success' or mode.startswith('write-'):
    sandbox = db.parent / 'native-write-responder'
    sandbox.mkdir(mode=0o700)
    work = sandbox / 'beads.db'
    for suffix in ('', '-wal', '-shm'):
        shutil.copyfile(str(db) + suffix, str(work) + suffix)
    probe = '__br_wal_probe_' + sandbox.name
    table = '"' + probe + '"'
    main_only = sandbox / 'main-only'
    if mode == 'write-symlink-directory':
        actual = sandbox / 'kept-main-only'
        actual.mkdir(mode=0o700)
        main_only.symlink_to(actual, target_is_directory=True)
    else:
        main_only.mkdir(mode=0o700)
    candidate = main_only / 'beads.db'
    with contextlib.closing(sqlite3.connect(work)) as c:
        c.execute('PRAGMA wal_autocheckpoint=0')
        c.execute('CREATE TABLE ' + table + '(id INTEGER PRIMARY KEY, note TEXT, payload BLOB)')
        c.execute('INSERT INTO ' + table + " VALUES(1, 'committed', ?)", (b'\0\xff' * 2048,))
        c.commit()
    with contextlib.closing(sqlite3.connect(work)) as c:
        c.execute('PRAGMA wal_autocheckpoint=0')
        assert c.execute('SELECT note FROM ' + table).fetchall() == [('committed',)]
        c.execute('UPDATE ' + table + " SET note='rolled back'")
        c.rollback()
        assert c.execute('SELECT note FROM ' + table).fetchall() == [('committed',)]
        c.execute('DROP TABLE ' + table)
        if mode == 'write-extra-schema':
            c.execute('CREATE TABLE unexpected(id INTEGER PRIMARY KEY)')
        if mode == 'write-metadata':
            c.execute('PRAGMA user_version=999')
        if mode == 'write-blob-loss':
            c.execute("UPDATE issues SET payload=X'00'")
        if mode == 'write-rowid-change':
            c.execute('UPDATE issues SET rowid=rowid+100')
        if mode == 'write-type-change':
            c.execute('UPDATE issues SET status=CAST(status AS BLOB)')
        c.commit()
        assert c.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0,0,0)
        data = db.read_bytes() if mode == 'write-lost-commit' else work.read_bytes()
        if mode == 'write-symlink-image':
            target = main_only / 'kept.db'
            target.write_bytes(data)
            candidate.symlink_to(target)
        else:
            candidate.write_bytes(data)
    prepared = dict(schema='br.orphaned_wal.native_write.v1',
                    manifest_sha256=reply['manifest_sha256'], input_family=str(db),
                    sql_write_target=str(work))
    if mode == 'write-wrong-input':
        prepared['input_family'] = '/not-the-private-family'
    (sandbox / 'prepared.json').write_text(json.dumps(prepared))
    proof = dict(schema='br.orphaned_wal.native_write.v1', verified=True,
                 scope='fresh-private-copy', source_checkpoint_performed=False,
                 manifest_sha256=reply['manifest_sha256'], sandbox=sandbox.name,
                 probe_table=probe, input_family_unchanged=True, committed_write_reopened=True,
                 rollback_verified=True, checkpoint=[0,0,0], main_only_verified=True,
                 main_only_bytes=len(data), main_only_sha256=hashlib.sha256(data).hexdigest())
    if mode == 'write-busy':
        proof['checkpoint'] = [1,0,0]
    if mode == 'write-boolean-checkpoint':
        proof['checkpoint'] = [False,0,0]
    if mode == 'write-wrong-manifest':
        proof['manifest_sha256'] = '0' * 64
    if mode == 'write-escape':
        proof['sandbox'] = 'native-write-../outside'
    if mode == 'write-wrong-hash':
        proof['main_only_sha256'] = '0' * 64
    if mode == 'write-shared-image':
        candidate.chmod(0o666)
    if mode == 'write-hardlink-image':
        os.link(candidate, main_only / 'extra-link')
    if mode == 'write-failed':
        (sandbox / 'failed.json').write_text('{}')
    complete = json.dumps(proof)
    if mode == 'write-disagree':
        complete = json.dumps(dict(proof, rollback_verified=False))
    if mode != 'write-missing-completion':
        (sandbox / 'complete.json').write_text(complete)
    text = json.dumps(proof)
    if mode == 'write-duplicate':
        text = text[:-1] + ',"verified":true}'
    if mode == 'write-nonfinite':
        text = text[:-1] + ',"extra":NaN}'
    (db.parent / 'native-write.json').write_text(text)
    if mode == 'write-check-directory-collision':
        collision = db.parent / 'independent-checkpoint'
        collision.mkdir(mode=0o700)
        (collision / 'beads.db').write_bytes(b'keep evidence')
if mode == 'wal-mutation':
    with open(str(db)+'-wal', 'r+b') as f:
        f.seek(60); f.write(b'wrong')
if mode == 'index-mutation':
    with open(str(db)+'-shm', 'r+b') as f:
        f.seek(150); f.write(b'wrong')
if mode == 'manifest-mutation':
    manifest.write_bytes(manifest.read_bytes()+b' ')
print(json.dumps(reply))
'''


class NativeGateControls(unittest.TestCase):
    def setUp(self):
        recovery.platform_check()
        self.root = Path(tempfile.mkdtemp(prefix='br-native-gate-'))
        self.db = make_orphan(self.root)
        self.output = self.root / 'output'
        self.verifier = self.root / "responder ' λ.py"
        self.verifier.write_text('#!' + sys.executable + '\n' + RESPONDER)
        self.verifier.chmod(0o700)
        self.verifier.with_suffix('.mode').write_text('success')
        self.original, self.ids = family(self.db), identity(self.db)

    def attempt(self, **kwargs):
        return recovery.recover_copy(self.db, self.output, engine_verifier=self.verifier, **kwargs)

    def refused(self, mode, pattern):
        self.verifier.with_suffix('.mode').write_text(mode)
        with self.assertRaisesRegex(recovery.RecoveryError, pattern):
            self.attempt(restore_shm=True)
        self.assertEqual(family(self.db), self.original)
        self.assertEqual(identity(self.db), self.ids)
        self.assertFalse(self.output.exists())
        self.assertTrue(list(self.root.glob('.br-wal-copy-*/failed.json')))

    def test_matching_responder_runs_before_source_install_under_private_locks(self):
        # Positive orchestration control only: the response explicitly labels itself.
        with mock.patch.dict(os.environ, {'BEADS_DIR': '/do-not-use', 'FSQLITE_TRUSTED_UNIX_DATABASE': '/do-not-use'}):
            report = self.attempt(restore_shm=True)
        self.assertEqual(report['native_probe']['engine_version'], 'TEST-RESPONDER-NOT-AN-ENGINE')
        self.assertTrue(report['native_probe']['protected_private_bytes_unchanged'])
        proof = report['native_probe']['independent_write']
        self.assertTrue(proof['verified'])
        self.assertEqual(proof['verifier'], 'stock-sqlite')
        directory = self.output / 'native-rehearsal'
        self.assertEqual(proof['native_receipt_sha256'], hashlib.sha256(
            (directory / 'native-write.json').read_bytes()).hexdigest())
        self.assertEqual(proof, json.loads((directory / 'independent-checkpoint/verified.json').read_text()))
        with contextlib.closing(sqlite3.connect(
                (directory / 'independent-checkpoint/beads.db').as_uri() + '?mode=ro&immutable=1', uri=True)) as conn:
            self.assertEqual(conn.execute(QUERY).fetchall(), EXPECTED)
        for suffix, value in self.original.items():
            if suffix != '-shm':
                self.assertEqual(family(self.db)[suffix], value)
                self.assertEqual(identity(self.db)[suffix], self.ids[suffix])
        manifest = json.loads((self.output / 'native-rehearsal/expected.json').read_text())
        self.assertEqual(len(manifest['tables']), 1)
        self.assertEqual(manifest['tables'][0]['name'], 'issues')
        self.assertEqual(manifest['tables'][0]['rows'], 1)

    def test_failed_engine_does_not_install_an_index(self):
        self.refused('exit', 'native engine rehearsal failed')

    def test_timeout_does_not_install_an_index(self):
        with mock.patch.object(recovery, 'NATIVE_PROBE_TIMEOUT', 0.1):
            self.refused('timeout', 'timed out')

    def test_malformed_output_cannot_be_success(self):
        self.refused('garbage', 'invalid native engine receipt')

    def test_duplicate_output_keys_cannot_be_success(self):
        self.refused('duplicate', 'duplicate')

    def test_nonfinite_output_cannot_be_success(self):
        self.refused('nonfinite', 'non-finite')

    def test_wrong_manifest_is_not_an_attestation(self):
        self.refused('wrong-manifest', 'does not attest')

    def test_numeric_one_is_not_verified_boolean(self):
        self.refused('unverified', 'does not attest')

    def test_unknown_engine_version_is_not_qualification(self):
        self.refused('unknown-engine', 'does not attest')

    def test_different_engine_is_not_qualification(self):
        self.refused('wrong-engine', 'does not attest')

    def test_undeclared_output_is_refused(self):
        self.refused('extra', 'does not attest')

    def test_success_response_cannot_hide_wal_mutation(self):
        self.refused('wal-mutation', 'protected component')

    def test_success_response_cannot_hide_index_mutation(self):
        self.refused('index-mutation', 'rewrote the proposed index')

    def test_success_response_cannot_rewrite_its_expectation(self):
        self.refused('manifest-mutation', 'expectation changed')

    def test_resume_probes_installed_index_and_keeps_old_evidence(self):
        old = self.root / 'old'
        interrupt_restore(self.db, old)
        before, ids, evidence = family(self.db), identity(self.db), evidence_bytes(old)
        report = self.attempt(resume_from=old)
        self.assertTrue(report['resume']['existing_index_revalidated'])
        self.assertIn('native_probe', report)
        self.assertEqual(family(self.db), before)
        self.assertEqual(identity(self.db), ids)
        self.assertEqual(evidence_bytes(old), evidence)

    def test_copy_only_cannot_claim_index_probe(self):
        with self.assertRaisesRegex(recovery.RecoveryError, 'only valid'):
            self.attempt()
        self.assertFalse(self.output.exists())
        self.assertEqual(family(self.db), self.original)

    def test_read_only_success_without_write_evidence_is_refused(self):
        self.refused('read-only', 'native-write.json')

    def test_matching_hashes_cannot_hide_a_lost_committed_wal_record(self):
        # The planted image is structurally valid and its receipt hashes match.
        self.refused('write-lost-commit', 'lost or changed')

    def test_native_write_must_preserve_schema_metadata_blob_rowid_and_types(self):
        for mode in ('write-extra-schema', 'write-metadata', 'write-blob-loss',
                     'write-rowid-change', 'write-type-change'):
            with self.subTest(mode=mode):
                self.refused(mode, 'lost or changed')

    def test_write_checkpoint_must_be_exact_integer_success(self):
        for mode in ('write-busy', 'write-boolean-checkpoint'):
            with self.subTest(mode=mode):
                self.refused(mode, 'checkpoint')

    def test_write_proof_must_bind_the_expected_manifest_and_private_input(self):
        self.refused('write-wrong-manifest', 'bound')
        self.refused('write-wrong-input', 'does not bind')

    def test_native_write_evidence_is_complete_and_not_failed(self):
        self.refused('write-missing-completion', 'complete.json')
        self.refused('write-disagree', 'disagrees')
        self.refused('write-failed', 'failure evidence')

    def test_native_write_json_is_strict(self):
        self.refused('write-duplicate', 'duplicate')
        self.refused('write-nonfinite', 'non-finite')

    def test_native_write_paths_cannot_escape_or_follow_links(self):
        self.refused('write-escape', 'unsafe.*sandbox')
        self.refused('write-symlink-directory', 'Not a directory|symbolic link')
        self.refused('write-symlink-image', 'symbolic link|Too many levels')
        self.refused('write-hardlink-image', 'hard-linked')

    def test_native_write_image_must_be_private_and_match_its_receipt(self):
        self.refused('write-shared-image', 'owner-controlled')
        self.refused('write-wrong-hash', 'differs from its receipt')

    def test_independent_checkpoint_directory_is_never_reused(self):
        self.refused('write-check-directory-collision', 'File exists')
        files = list(self.root.glob('.br-wal-copy-*/native-rehearsal/independent-checkpoint/beads.db'))
        self.assertEqual(len(files), 1)
        self.assertEqual(files[0].read_bytes(), b'keep evidence')

    def test_checkpoint_evidence_inode_exchange_is_detected(self):
        native_manifest = recovery.native_manifest
        def exchanged(path, *args, **kwargs):
            result = native_manifest(path, *args, **kwargs)
            if kwargs.get('immutable'):
                image = path.parent.parent / 'native-write-responder/main-only/beads.db'
                preserved = image.with_name('preserved-inode.db')
                image.rename(preserved)
                image.write_bytes(preserved.read_bytes())
            return result
        with mock.patch.object(recovery, 'native_manifest', side_effect=exchanged):
            self.refused('success', 'evidence.*changed')

    def test_failed_independent_resume_preserves_the_installed_index_and_prior_evidence(self):
        old = self.root / 'old'
        interrupt_restore(self.db, old)
        before, ids, prior = family(self.db), identity(self.db), evidence_bytes(old)
        self.verifier.with_suffix('.mode').write_text('write-lost-commit')
        with self.assertRaisesRegex(recovery.RecoveryError, 'lost or changed'):
            self.attempt(resume_from=old)
        self.assertEqual(family(self.db), before)
        self.assertEqual(identity(self.db), ids)
        self.assertEqual(evidence_bytes(old), prior)
        self.assertFalse(self.output.exists())

    def test_wire_protocol_matches_native_test_vector(self):
        encoded = recovery.native_row([None, -2, -0.0, 'λ', b'\0\xff'])
        self.assertEqual(encoded.hex(),
            '00000000000000056e69fffffffffffffffe728000000000000000740000000000000002cebb62000000000000000200ff')
        self.assertNotEqual(recovery.native_row([1]), recovery.native_row([1.0]))
        self.assertNotEqual(recovery.native_row(['x']), recovery.native_row([b'x']))
        self.assertNotEqual(recovery.native_row(['ab','c']), recovery.native_row(['a','bc']))

    def test_row_hash_is_order_independent_but_preserves_multiplicity(self):
        rows = [(2, 'x'), (1, b'\0')]
        self.assertEqual(recovery.native_rows_digest(rows, 1024), recovery.native_rows_digest(rows[::-1], 1024))
        self.assertNotEqual(recovery.native_rows_digest(rows, 1024), recovery.native_rows_digest(rows*2, 1024))
        with self.assertRaisesRegex(recovery.RecoveryError, 'exceeds'):
            recovery.native_rows_digest(rows, 1)


def qualify_native(binary: Path):
    """No responder or Python fallback: source mutations require the compiled engine."""
    binary = binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise AssertionError('a real compiled companion is required')
    root = Path(tempfile.mkdtemp(prefix='br-native-gate-binary-'))
    # Move execution away from the checkout, keeping source and artifacts separate.
    installed = root / 'installed-companion'
    shutil.copy2(binary, installed)
    source = make_orphan(root)
    before = family(source)
    output = root / 'verified'
    result = subprocess.run([str(installed), '--db', str(source), '--output', str(output), '--restore-shm'],
                            capture_output=True, text=True, timeout=90, check=False)
    (root/'native.stdout').write_text(result.stdout)
    (root/'native.stderr').write_text(result.stderr)
    if result.returncode != 0:
        raise AssertionError(f'actual native restoration failed: {result.stderr}; evidence: {root}')
    report = json.loads(result.stdout)
    proof = report['native_probe']
    assert proof['engine'] == 'frankensqlite' and proof['verified'] is True
    assert proof['engine_version'] not in ('', 'unknown', 'TEST-RESPONDER-NOT-AN-ENGINE')
    assert proof['executable_sha256'] == hashlib.sha256(installed.read_bytes()).hexdigest()
    independent = proof['independent_write']
    assert independent['schema'] == 'br.orphaned_wal.native_write_check.v1'
    assert independent['verified'] is True and independent['verifier'] == 'stock-sqlite'
    assert independent['manifest_sha256'] == proof['manifest_sha256']
    observed = output / 'native-rehearsal/independent-checkpoint/beads.db'
    assert hashlib.sha256(observed.read_bytes()).hexdigest() == independent['main_only_sha256']
    with contextlib.closing(sqlite3.connect(observed.as_uri() + '?mode=ro&immutable=1', uri=True)) as conn:
        assert conn.execute(QUERY).fetchall() == EXPECTED
    for suffix in before:
        if suffix != '-shm':
            assert family(source)[suffix] == before[suffix]
    # Do not let a stock SQLite source read precede the gate under qualification.
    with contextlib.closing(sqlite3.connect(source)) as conn:
        assert conn.execute(QUERY).fetchall() == EXPECTED
    print(f'Actual compiled FrankenSQLite and independent checkpoint attestation passed: {root}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--recovery-binary', type=Path)
    args, rest = parser.parse_known_args()
    if args.recovery_binary:
        if rest:
            parser.error('unexpected arguments')
        qualify_native(args.recovery_binary)
    else:
        print('CONTROL TESTS: fake responder; no compiled FrankenSQLite binary is tested.', flush=True)
        unittest.main(argv=[sys.argv[0], *rest], verbosity=2)
