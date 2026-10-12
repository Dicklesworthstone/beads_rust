"""Current-source and recovered-output checks against separately trusted snapshots.

All inputs are new temporary fixtures, retained for inspection. No production
tracker, Git index, or engine is opened by the comparison implementation.
"""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "br_runtime_snapshot.py"
SPEC = importlib.util.spec_from_file_location("snapshot_compare", SCRIPT)
snapshot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snapshot)


@unittest.skipUnless(os.name == "posix", "descriptor-relative POSIX implementation")
class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-snapshot-compare-tests-"))
        self.metadata = self.root / "metadata"
        self.database = self.root / "external-db"
        self.metadata.mkdir()
        self.database.mkdir()
        (self.metadata / "issues.jsonl").write_bytes(b'{"id":"bd-local"}\n')
        (self.metadata / "empty").mkdir()
        (self.database / "beads.db").write_bytes(b"local main file")
        (self.database / "beads.db-wal").write_bytes(b"local WAL work")
        (self.database / "beads.db-wal-cert").write_bytes(b"opaque certificate bytes")
        self.bundle = self.root / "bundle"
        self.output = self.root / "recovered"
        self.sources = [("database", self.database), ("metadata", self.metadata)]

    def capture(self, is_set=True):
        if is_set:
            return snapshot.capture_set(self.sources, self.bundle, writers_stopped=True)["set_sha256"]
        return snapshot.capture(self.database, self.bundle, writers_stopped=True)["manifest_sha256"]

    def recover(self, digest, is_set=True):
        if is_set:
            snapshot.materialize_set(self.bundle, self.output, digest)
        else:
            snapshot.materialize(self.bundle, self.output, digest)

    def check_source(self, digest, is_set=True, **kwargs):
        return snapshot.check_sources(self.bundle, digest, is_set=is_set, writers_stopped=True, **kwargs)

    def check_recovered(self, digest, is_set=True, **kwargs):
        return snapshot.check_recovery(self.bundle, self.output, digest, is_set=is_set,
                                       writers_stopped=True, **kwargs)

    def witness(self):
        # Include bundle, receipt, all compared roots and unrelated sentinel
        # paths. Atime is intentionally absent: opening for read can update it.
        result = {}
        for path in [self.root, *self.root.rglob("*")]:
            info = path.lstat()
            result[os.fsencode(path.relative_to(self.root))] = (
                snapshot.identity(info), path.read_bytes() if stat.S_ISREG(info.st_mode) else None)
        return result

    def mutate_byte(self, path):
        info = path.stat()
        parent_before = snapshot.identity(path.parent.stat())
        with path.open("r+b") as file:
            first = file.read(1)
            self.assertTrue(first)
            file.seek(0)
            file.write(bytes([first[0] ^ 1]))
        # Even unchanged mtime, length, inode and directory metadata must not
        # hide a content change made since the original snapshot.
        os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns))
        self.assertEqual(snapshot.identity(path.parent.stat()), parent_before)

    def assert_match(self, result, compared, members):
        self.assertEqual(result["schema"], "br.runtime-comparison.v1")
        self.assertEqual(result["status"], "matches_snapshot")
        self.assertEqual(result["compared"], compared)
        self.assertEqual(len(result["members"]), members)
        for flag in ("comparison_atomic", "capture_inode_continuity_verified",
                     "engine_validated", "source_writes_performed", "live_installation_performed"):
            self.assertIs(result[flag], False)

    def test_single_original_and_recovery_match_without_writes(self):
        digest = self.capture(False)
        self.recover(digest, False)
        before = self.witness()
        with mock.patch.object(snapshot, "new_file", side_effect=AssertionError("no output writes")):
            self.assert_match(self.check_source(digest, False), "original_sources", 1)
            self.assert_match(self.check_recovered(digest, False), "recovered_directories", 1)
        self.assertEqual(self.witness(), before)

    def test_set_originals_and_recovery_match_without_writes(self):
        digest = self.capture()
        self.recover(digest)
        before = self.witness()
        with mock.patch.object(snapshot, "external_output", side_effect=AssertionError("no output creation")):
            self.assert_match(self.check_source(digest), "original_sources", 2)
            self.assert_match(self.check_recovered(digest), "recovered_directories", 2)
        self.assertEqual(self.witness(), before)

    def test_empty_original_and_recovered_directory_match_with_zero_byte_budget(self):
        empty = self.root / "empty-source"
        empty.mkdir()
        digest = snapshot.capture(empty, self.bundle, writers_stopped=True, max_bytes=0)["manifest_sha256"]
        snapshot.materialize(self.bundle, self.output, digest, max_bytes=0)
        before = self.witness()
        result = self.check_source(digest, False, max_bytes=0)
        self.assertEqual(result["members"][0]["files"], 0)
        self.check_recovered(digest, False, max_bytes=0)
        self.assertEqual(self.witness(), before)

    def test_valid_bundle_is_not_evidence_that_sources_still_match(self):
        digest = self.capture()
        self.mutate_byte(self.database / "beads.db-wal")
        snapshot.verify_set(self.bundle, digest)
        before = self.witness()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_digest_mismatch"):
            self.check_source(digest)
        self.assertEqual(self.witness(), before)

    def test_valid_recovery_receipt_is_not_evidence_that_recovered_bytes_still_match(self):
        digest = self.capture()
        self.recover(digest)
        marker = (self.output / "recovery.json").read_bytes()
        self.mutate_byte(self.output / "data" / "database" / "beads.db-wal-cert")
        snapshot.verify_set(self.bundle, digest)
        before = self.witness()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_digest_mismatch"):
            self.check_recovered(digest)
        self.assertEqual((self.output / "recovery.json").read_bytes(), marker)
        self.assertEqual(self.witness(), before)

    def test_original_namespace_addition_is_not_covered_by_a_valid_old_snapshot(self):
        digest = self.capture()
        (self.database / "future-wal-sidecar").write_bytes(b"new committed evidence")
        before = self.witness()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_inventory_mismatch"):
            self.check_source(digest)
        self.assertEqual(self.witness(), before)

    def test_original_absence_of_empty_directory_and_file_are_both_checked(self):
        digest = self.capture()
        for leaf in (self.metadata / "empty", self.database / "beads.db-wal"):
            saved = self.root / "retained"
            leaf.rename(saved)
            with self.subTest(path=leaf), self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_mismatch"):
                self.check_source(digest)
            saved.rename(leaf)
        self.check_source(digest)

    def test_recovered_extra_missing_and_wrong_kind_are_refused(self):
        digest = self.capture()
        self.recover(digest)
        data = self.output / "data" / "database"
        original = data / "beads.db-wal"
        original.rename(self.root / "retained-wal")
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_mismatch"):
            self.check_recovered(digest)
        original.mkdir(mode=0o700)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "kind_mismatch"):
            self.check_recovered(digest)
        original.rename(self.root / "retained-directory")
        (self.root / "retained-wal").rename(original)
        (data / "unexpected").write_bytes(b"extra")
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_mismatch"):
            self.check_recovered(digest)

    def test_recovery_checks_complete_wrapper_and_set_members(self):
        digest = self.capture()
        self.recover(digest)
        (self.output / "data" / "unexpected-member").mkdir()
        with self.assertRaises(snapshot.SnapshotFailure):
            self.check_recovered(digest)
        (self.output / "data" / "unexpected-member").rename(self.root / "held-member")
        (self.output / "extra-receipt").write_bytes(b"unexpected")
        with self.assertRaises(snapshot.SnapshotFailure):
            self.check_recovered(digest)

    def test_quiescence_assertion_is_required_on_all_comparisons(self):
        digest = self.capture()
        self.recover(digest)
        before = self.witness()
        for is_set in (False, True):
            for method, args in ((snapshot.check_sources, (self.bundle, digest)),
                                 (snapshot.check_recovery, (self.bundle, self.output, digest))):
                with self.subTest(is_set=is_set, method=method), self.assertRaisesRegex(snapshot.SnapshotFailure, "stop_all"):
                    method(*args, is_set=is_set)
        self.assertEqual(self.witness(), before)

    def test_corrupt_bundle_is_refused_before_any_compared_root_is_opened(self):
        digest = self.capture()
        self.recover(digest)
        original = snapshot.directory

        @contextlib.contextmanager
        def guarded(path):
            self.assertNotIn(path, (self.database, self.metadata, self.output))
            with original(path) as root:
                yield root

        with mock.patch.object(snapshot, "directory", side_effect=guarded):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
                self.check_source("0" * 64)
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
                self.check_recovered("0" * 64)
        snapshot.verify_set(self.bundle, digest)

    def test_corrupt_saved_payload_refuses_even_when_current_source_is_healthy(self):
        digest = self.capture()
        saved = self.bundle / "members" / "metadata"
        manifest = json.loads((saved / "manifest.json").read_bytes())
        record = next(item for item in manifest["entries"] if item["kind"] == "file")
        self.mutate_byte(saved / "payload" / record["object"])
        original = snapshot.directory

        @contextlib.contextmanager
        def guarded(path):
            self.assertNotIn(path, (self.database, self.metadata))
            with original(path) as root:
                yield root

        with mock.patch.object(snapshot, "directory", side_effect=guarded):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "payload_digest"):
                self.check_source(digest)

    def test_even_a_rehashed_manifest_cannot_supply_an_invalid_original_path(self):
        self.capture(False)
        path = self.bundle / "manifest.json"
        original = json.loads(path.read_bytes())
        for raw_source in (b"/tmp/has\0nul", b"/tmp//ambiguous", b"/tmp/../elsewhere"):
            changed = dict(original)
            changed["source_path_hex"] = raw_source.hex()
            raw = snapshot.encode_manifest(changed)
            path.write_bytes(raw)
            expected = hashlib.sha256(raw).hexdigest()
            with self.subTest(source=raw_source), contextlib.redirect_stdout(io.StringIO()) as stdout:
                code = snapshot.main(["check-source", str(self.bundle), "--manifest-sha256",
                                      expected, "--writers-stopped"])
            self.assertEqual(code, 2)
            self.assertEqual(json.loads(stdout.getvalue())["status"], "refused_or_incomplete")

    def test_missing_original_is_not_read_during_recovery_check(self):
        digest = self.capture()
        self.recover(digest)
        self.database.rename(self.root / "old-database")
        self.metadata.rename(self.root / "old-metadata")
        before = self.witness()
        self.check_recovered(digest)
        with self.assertRaises(FileNotFoundError):
            self.check_source(digest)
        self.assertEqual(self.witness(), before)

    def test_replacement_original_inodes_do_not_claim_capture_identity_continuity(self):
        digest = self.capture()
        held = self.root / "held-database"
        self.database.rename(held)
        shutil.copytree(held, self.database)
        self.assertNotEqual(self.database.stat().st_ino, held.stat().st_ino)
        result = self.check_source(digest)
        self.assertIs(result["capture_inode_continuity_verified"], False)

    def test_source_and_recovered_permission_changes_are_not_silently_accepted(self):
        digest = self.capture()
        self.recover(digest)
        for path, check in ((self.database / "beads.db-wal", self.check_source),
                            (self.metadata / "empty", self.check_source),
                            (self.output / "data" / "database" / "beads.db", self.check_recovered),
                            (self.output / "data" / "metadata", self.check_recovered),
                            (self.output, self.check_recovered),
                            (self.output / "data", self.check_recovered),
                            (self.output / "recovery.json", self.check_recovered)):
            mode = stat.S_IMODE(path.stat().st_mode)
            path.chmod(mode ^ 0o040)
            with self.subTest(path=path), self.assertRaisesRegex(snapshot.SnapshotFailure, "permissions"):
                check(digest)
            path.chmod(mode)
        self.check_source(digest)
        self.check_recovered(digest)

    def test_special_permission_bits_are_not_certified_as_ordinary_rwx(self):
        digest = self.capture()
        (self.database / "beads.db").chmod(0o4644)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "permissions"):
            self.check_source(digest)

    def test_compared_leaf_symlink_hardlink_and_fifo_are_refused(self):
        digest = self.capture(False)
        leaf = self.database / "beads.db"
        held = self.root / "held-main"
        leaf.rename(held)
        factories = (lambda: leaf.symlink_to(held), lambda: os.link(held, leaf), lambda: os.mkfifo(leaf))
        for i, make in enumerate(factories):
            make()
            with self.subTest(kind=i), self.assertRaises((snapshot.SnapshotFailure, OSError)):
                self.check_source(digest, False)
            leaf.rename(self.root / f"held-special-{i}")

    def test_compared_root_and_ancestor_symlinks_are_not_followed(self):
        digest = self.capture()
        self.recover(digest)
        self.database.rename(self.root / "original-db")
        self.database.symlink_to(self.root / "original-db", target_is_directory=True)
        with self.assertRaises(OSError):
            self.check_source(digest)
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(OSError):
            snapshot.check_recovery(self.bundle, alias / "recovered", digest,
                                    is_set=True, writers_stopped=True)

    def test_recovery_marker_wrong_hash_unknown_fields_and_type_changes_are_refused(self):
        digest = self.capture()
        self.recover(digest)
        marker = self.output / "recovery.json"
        original = json.loads(marker.read_bytes())
        for key, value in (("set_sha256", "0" * 64), ("engine_validated", 0),
                           ("engine_validated", True), ("unverified_field", True),
                           ("data_directory", str(self.database))):
            changed = dict(original)
            changed[key] = value
            marker.write_bytes(snapshot.encode_manifest(changed))
            with self.subTest(key=key, value=value), self.assertRaisesRegex(snapshot.SnapshotFailure, "receipt_mismatch"):
                self.check_recovered(digest)
        marker.write_bytes(snapshot.encode_manifest(original))
        self.check_recovered(digest)

    def test_duplicate_and_missing_recovery_marker_refuse(self):
        digest = self.capture()
        self.recover(digest)
        marker = self.output / "recovery.json"
        raw = marker.read_bytes()
        marker.write_bytes(raw.replace(b'"schema":', b'"schema":"extra","schema":', 1))
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "duplicate"):
            self.check_recovered(digest)
        marker.rename(self.root / "held-receipt")
        with self.assertRaises(snapshot.SnapshotFailure):
            self.check_recovered(digest)

    def test_partial_recovery_with_no_receipt_never_matches(self):
        digest = self.capture()
        self.output.mkdir(mode=0o700)
        (self.output / "data").mkdir(mode=0o700)
        before = self.witness()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "recovery_inventory"):
            self.check_recovered(digest)
        self.assertEqual(self.witness(), before)

    def test_mutation_to_early_source_while_later_source_is_hashed_refuses(self):
        digest = self.capture()
        trigger = snapshot.identity((self.metadata / "issues.jsonl").stat())[:2]
        stream = snapshot.stream
        changed = []

        def mutate(file, size, budget, destination=None):
            result = stream(file, size, budget, destination)
            if snapshot.identity(os.fstat(file))[:2] == trigger and not changed:
                self.mutate_byte(self.database / "beads.db-wal")
                changed.append(True)
            return result

        with mock.patch.object(snapshot, "stream", side_effect=mutate):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_changed"):
                self.check_source(digest)
        self.assertEqual(changed, [True])
        snapshot.verify_set(self.bundle, digest)

    def test_mutation_to_early_recovery_while_later_recovery_is_hashed_refuses(self):
        digest = self.capture()
        self.recover(digest)
        target = self.output / "data"
        trigger = snapshot.identity((target / "metadata" / "issues.jsonl").stat())[:2]
        stream = snapshot.stream
        changed = []

        def mutate(file, size, budget, destination=None):
            result = stream(file, size, budget, destination)
            if snapshot.identity(os.fstat(file))[:2] == trigger and not changed:
                self.mutate_byte(target / "database" / "beads.db")
                changed.append(True)
            return result

        with mock.patch.object(snapshot, "stream", side_effect=mutate):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_changed"):
                self.check_recovered(digest)
        self.assertEqual(changed, [True])

    def test_changes_during_the_last_bundle_verification_are_not_too_late_to_detect(self):
        digest = self.capture()
        self.recover(digest)
        verifier = snapshot.verified_set
        for target, method in ((self.database / "beads.db-wal", self.check_source),
                               (self.output / "data" / "database" / "beads.db", self.check_recovered)):
            @contextlib.contextmanager
            def mutate_on_exit(*args):
                with verifier(*args) as saved:
                    yield saved
                self.mutate_byte(target)
            with mock.patch.object(snapshot, "verified_set", side_effect=mutate_on_exit):
                with self.subTest(target=target), self.assertRaisesRegex(snapshot.SnapshotFailure, "comparison_changed"):
                    method(digest)

    def test_recovery_receipt_changed_during_final_bundle_check_refuses(self):
        digest = self.capture()
        self.recover(digest)
        verifier = snapshot.verified_set

        @contextlib.contextmanager
        def mutate_on_exit(*args):
            with verifier(*args) as saved:
                yield saved
            path = self.output / "recovery.json"
            result = json.loads(path.read_bytes())
            result["engine_validated"] = True
            path.write_bytes(snapshot.encode_manifest(result))

        with mock.patch.object(snapshot, "verified_set", side_effect=mutate_on_exit):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "receipt_mismatch"):
                self.check_recovered(digest)

    def test_compared_root_replacement_after_hashing_refuses(self):
        digest = self.capture(False)
        verifier = snapshot.verified_bundle

        @contextlib.contextmanager
        def replace_on_exit(*args):
            with verifier(*args) as saved:
                yield saved
            self.database.rename(self.root / "retained-original")
            shutil.copytree(self.root / "retained-original", self.database)

        with mock.patch.object(snapshot, "verified_bundle", side_effect=replace_on_exit):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "directory_identity_changed"):
                self.check_source(digest, False)

    def test_non_utf8_paths_and_empty_directories_roundtrip(self):
        unusual = os.fsencode(self.database) + b"/name-\xff\n"
        with open(unusual, "wb") as file:
            file.write(b"opaque exact bytes")
        digest = self.capture()
        self.recover(digest)
        before = self.witness()
        self.check_source(digest)
        self.check_recovered(digest)
        self.assertEqual(self.witness(), before)

    def test_aggregate_byte_and_entry_limits_apply_before_comparison_success(self):
        digest = self.capture()
        self.recover(digest)
        single = max(sum(p.stat().st_size for p in source.rglob("*") if p.is_file())
                     for _name, source in self.sources)
        for method in (self.check_source, self.check_recovered):
            with self.subTest(method=method), self.assertRaisesRegex(snapshot.SnapshotFailure, "byte_limit"):
                method(digest, max_bytes=single)
            with mock.patch.object(snapshot, "MAX_ENTRIES", 5):
                with self.assertRaises(snapshot.SnapshotFailure):
                    method(digest)

    def test_invalid_budgets_and_expired_deadline_refuse_without_changes(self):
        digest = self.capture()
        self.recover(digest)
        before = self.witness()
        for method in (self.check_source, self.check_recovered):
            for timeout in (0, -1, float("nan"), float("inf")):
                with self.subTest(method=method, timeout=timeout), self.assertRaises(snapshot.SnapshotFailure):
                    method(digest, timeout=timeout)
            with mock.patch.object(snapshot.time, "monotonic", side_effect=[0, 5]):
                with self.assertRaisesRegex(snapshot.SnapshotFailure, "deadline"):
                    method(digest, timeout=1)
        self.assertEqual(self.witness(), before)

    def test_all_four_cli_commands_require_assertion_and_emit_read_only_matches(self):
        digest = self.capture()
        self.recover(digest)
        single = self.root / "single"
        one = snapshot.capture(self.database, single, writers_stopped=True)["manifest_sha256"]
        recovered = self.root / "single-recovered"
        snapshot.materialize(single, recovered, one)
        cases = [("check-source", single, None, "--manifest-sha256", one),
                 ("check-recovery", single, recovered, "--manifest-sha256", one),
                 ("check-source-set", self.bundle, None, "--set-sha256", digest),
                 ("check-recovery-set", self.bundle, self.output, "--set-sha256", digest)]
        before = self.witness()
        for name, bundle, output, flag, expected in cases:
            args = [name, str(bundle), *([str(output)] if output else []), flag, expected]
            for stopped in (False, True):
                with self.subTest(command=name, stopped=stopped), contextlib.redirect_stdout(io.StringIO()) as stdout:
                    code = snapshot.main(args + (["--writers-stopped"] if stopped else []))
                result = json.loads(stdout.getvalue())
                self.assertEqual(code, 0 if stopped else 2)
                self.assertEqual(result["status"], "matches_snapshot" if stopped else "refused_or_incomplete")
                self.assertIs(result["source_writes_performed"], False)
                self.assertIs(result["output_writes_performed"], False)
                if not stopped:
                    self.assertIs(result["partial_output_retained"], False)
        self.assertEqual(self.witness(), before)

    def test_real_cli_rejects_new_committed_wal_work_not_in_the_saved_set(self):
        # The writer process exits without SQLite close's final checkpoint.
        # All database users have exited before either capture or comparison.
        db = self.database / "actual.sqlite"
        writer = '''
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("PRAGMA journal_mode=WAL")
c.execute("PRAGMA wal_autocheckpoint=0")
c.execute("CREATE TABLE IF NOT EXISTS work (value TEXT)")
c.commit()
if sys.argv[2] == "first": c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
c.execute("INSERT INTO work VALUES (?)", (sys.argv[2],))
c.commit()
os._exit(0)
'''
        def append(value):
            result = subprocess.run([sys.executable, "-c", writer, str(db), value],
                                    capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
        append("first")
        digest = self.capture()
        self.recover(digest)
        append("second")
        snapshot.verify_set(self.bundle, digest)
        before = self.witness()
        result = subprocess.run([sys.executable, str(SCRIPT), "check-source-set", str(self.bundle),
                                 "--set-sha256", digest, "--writers-stopped"],
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "refused_or_incomplete")
        self.check_recovered(digest)
        self.assertEqual(self.witness(), before)
        # The independent saved generation really lacks the newer commit.
        with contextlib.closing(sqlite3.connect(self.output / "data" / "database" / "actual.sqlite")) as saved:
            self.assertEqual(saved.execute("SELECT value FROM work").fetchall(), [("first",)])
        with contextlib.closing(sqlite3.connect(db)) as live:
            self.assertEqual(live.execute("SELECT value FROM work").fetchall(), [("first",), ("second",)])


if __name__ == "__main__":
    unittest.main()
