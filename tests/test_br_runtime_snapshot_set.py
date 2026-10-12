"""Complete offline snapshot-set tests, using only retained synthetic roots.

No production tracker is opened. Faults are placed at actual capture/copy
boundaries; exact bytes and identities constrain both success and refusal.
"""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "br_runtime_snapshot.py"
SPEC = importlib.util.spec_from_file_location("snapshot_set", SCRIPT)
snapshot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snapshot)


def tree(path):
    return {os.fsencode(item.relative_to(path)):
            ("directory", None) if item.is_dir() else ("file", item.read_bytes())
            for item in path.rglob("*")}


def witness(path):
    return {os.fsencode(item.relative_to(path)):
            (snapshot.identity(item.stat()), None if item.is_dir() else item.read_bytes())
            for item in [path, *path.rglob("*")]}


@unittest.skipUnless(os.name == "posix", "descriptor-relative POSIX implementation")
class SnapshotSetTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-runtime-set-tests-"))
        self.metadata = self.root / "clone" / ".beads"
        self.metadata.mkdir(parents=True)
        (self.metadata.parent / ".git").mkdir()
        self.database = self.root / "cache"
        self.database.mkdir()
        (self.metadata / "metadata.json").write_text('{"database":"../../cache/tracker"}\n')
        (self.metadata / "issues.jsonl").write_bytes(b'{"id":"bd-new"}\n')
        (self.metadata / ".br_recovery" / "old" / "empty").mkdir(parents=True)
        (self.metadata / ".br_recovery" / "old" / "receipt.json").write_bytes(b"evidence")
        for name, data in {"tracker": b"database\0", "tracker-wal": b"pending commits",
                           "tracker-wal-cert": b"opaque certificate",
                           "tracker-wal-cert-head": b"opaque certificate head",
                           "future.sidecar": b"preserve unknown state"}.items():
            (self.database / name).write_bytes(data)
        self.sources = [("metadata", self.metadata), ("database", self.database)]
        self.bundle = self.root / "bundle"
        self.output = self.root / "recovered"

    def capture(self, **options):
        return snapshot.capture_set(self.sources, self.bundle, writers_stopped=True, **options)

    def source_witnesses(self):
        return {name: witness(path) for name, path in self.sources}

    def records(self):
        return json.loads((self.bundle / "set.json").read_bytes())["members"]

    def saved_file(self, name="database"):
        directory = self.bundle / "members" / name
        entries = json.loads((directory / "manifest.json").read_bytes())["entries"]
        entry = next(entry for entry in entries if entry["kind"] == "file")
        return directory / "payload" / entry["object"]

    def test_set_roundtrip_preserves_external_database_and_metadata_together(self):
        before = self.source_witnesses()
        receipt = self.capture()
        self.assertEqual(self.source_witnesses(), before)
        saved_before = witness(self.bundle)
        verified = snapshot.verify_set(self.bundle, receipt["set_sha256"])
        self.assertEqual([r["name"] for r in verified["members"]], ["database", "metadata"])
        result = snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        for name, path in self.sources:
            self.assertEqual(tree(self.output / "data" / name), tree(path))
        self.assertFalse((self.output / "data" / "metadata" / "tracker").exists())
        self.assertFalse(result["engine_validated"])
        self.assertFalse(result["capture_atomic"])
        self.assertFalse(result["live_installation_performed"])
        self.assertEqual(result, json.loads((self.output / "recovery.json").read_bytes()))
        self.assertEqual(self.source_witnesses(), before)
        self.assertEqual(witness(self.bundle), saved_before)

    def test_every_source_is_required_before_output_creation(self):
        self.sources.append(("missing", self.root / "missing"))
        with self.assertRaises(FileNotFoundError):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_set_capture_requires_one_explicit_stopped_interval(self):
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "stop_all"):
            snapshot.capture_set(self.sources, self.bundle)
        self.assertFalse(self.bundle.exists())

    def test_duplicate_invalid_and_overlapping_sources_are_refused(self):
        invalid = [[], [("metadata", self.metadata), ("metadata", self.database)],
                   [("a", self.metadata), ("b", self.metadata)],
                   [("a", self.metadata), ("b", self.metadata / ".br_recovery")]]
        invalid += [[(name, self.metadata)] for name in ["../bad", "", "UPPER", "a/b", "a\\b", ".git", "a" * 49]]
        for sources in invalid:
            with self.subTest(sources=sources), self.assertRaises(snapshot.SnapshotFailure):
                snapshot.capture_set(sources, self.bundle, writers_stopped=True)
        self.assertFalse(self.bundle.exists())

    def test_symlink_aliases_are_not_another_independent_source(self):
        alias = self.root / "cache-alias"
        alias.symlink_to(self.database, target_is_directory=True)
        self.sources.append(("alias", alias))
        with self.assertRaises(OSError):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_output_is_excluded_from_every_source_not_just_metadata(self):
        before = self.source_witnesses()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "overlap"):
            snapshot.capture_set(self.sources, self.database / "saved", writers_stopped=True)
        self.assertEqual(self.source_witnesses(), before)

    def test_root_count_is_bounded(self):
        with mock.patch.object(snapshot, "MAX_SET_MEMBERS", 1), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_aggregate_byte_budget_applies_before_any_capture(self):
        biggest = max(sum(item.stat().st_size for item in path.rglob("*") if item.is_file())
                      for _, path in self.sources)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "byte_limit"):
            self.capture(max_bytes=biggest)
        self.assertFalse(self.bundle.exists())

    def test_aggregate_entry_budget_counts_all_roots(self):
        biggest = max(1 + len(list(path.rglob("*"))) for _, path in self.sources)
        with mock.patch.object(snapshot, "MAX_ENTRIES", biggest), self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_limit"):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_individual_receipts_do_not_establish_set_wide_stability(self):
        # Counterfactual: both existing single-directory receipts still verify
        # after a change that straddles the two individually successful captures.
        first = self.root / "individual-database"
        second = self.root / "individual-metadata"
        a = snapshot.capture(self.database, first, writers_stopped=True)
        (self.database / "tracker-wal-cert").write_bytes(b"later certificate generation")
        b = snapshot.capture(self.metadata, second, writers_stopped=True)
        snapshot.verify(first, a["manifest_sha256"])
        snapshot.verify(second, b["manifest_sha256"])
        self.assertNotEqual((self.database / "tracker-wal-cert").read_bytes(), b"opaque certificate")

        original = snapshot._capture
        calls = []

        def change_early_source(source, bundle, budget):
            result = original(source, bundle, budget)
            calls.append(source)
            if len(calls) == 2:
                (self.database / "tracker-wal-cert").write_bytes(b"third certificate generation")
            return result

        with mock.patch.object(snapshot, "_capture", side_effect=change_early_source), self.assertRaisesRegex(snapshot.SnapshotFailure, "source_changed"):
            self.capture()
        self.assertEqual(len(calls), 2)
        self.assertTrue((self.bundle / "members" / "metadata" / "manifest.json").exists())
        self.assertFalse((self.bundle / "set.json").exists())

    def test_late_namespace_addition_invalidates_earlier_member(self):
        original = snapshot._capture

        def add(source, bundle, budget):
            result = original(source, bundle, budget)
            if source == self.metadata:
                (self.database / "new-sidecar").write_bytes(b"not captured")
            return result

        with mock.patch.object(snapshot, "_capture", side_effect=add), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertFalse((self.bundle / "set.json").exists())

    def test_replaced_early_root_is_refused_even_with_identical_payloads(self):
        original = snapshot._capture

        def replace(source, bundle, budget):
            result = original(source, bundle, budget)
            if source == self.metadata:
                contents = tree(self.database)
                self.database.rename(self.root / "retained-cache")
                self.database.mkdir()
                for path, (kind, data) in contents.items():
                    self.assertEqual(kind, "file")
                    (self.database / os.fsdecode(path)).write_bytes(data)
            return result

        with mock.patch.object(snapshot, "_capture", side_effect=replace), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertFalse((self.bundle / "set.json").exists())

    def test_failed_later_member_does_not_publish_a_set_receipt(self):
        before = self.source_witnesses()
        original = snapshot._capture

        def fail(source, bundle, budget):
            if source == self.metadata:
                raise OSError("second member failed")
            return original(source, bundle, budget)

        with mock.patch.object(snapshot, "_capture", side_effect=fail), self.assertRaises(OSError):
            self.capture()
        self.assertTrue((self.bundle / "members" / "database" / "manifest.json").exists())
        self.assertFalse((self.bundle / "set.json").exists())
        self.assertEqual(self.source_witnesses(), before)
        with self.assertRaises(snapshot.SnapshotFailure):
            snapshot.verify_set(self.bundle, "0" * 64)

    def test_all_members_share_a_deadline_instead_of_resetting_timeouts(self):
        original = snapshot._capture
        now = [0.0]
        observed = []

        def consume(source, bundle, budget):
            observed.append(budget.deadline)
            result = original(source, bundle, budget)
            now[0] = 2.0
            return result

        with mock.patch.object(snapshot.time, "monotonic", side_effect=lambda: now[0]), mock.patch.object(snapshot, "_capture", side_effect=consume):
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "deadline"):
                self.capture(timeout=1)
        self.assertEqual(observed, [1.0])
        self.assertFalse((self.bundle / "set.json").exists())

    def test_set_digest_is_a_separately_required_trust_anchor(self):
        receipt = self.capture()
        for digest in ["0" * 64, "", None, "x" * 64]:
            with self.subTest(digest=digest), self.assertRaises(snapshot.SnapshotFailure):
                snapshot.materialize_set(self.bundle, self.output, digest)
            self.assertFalse(self.output.exists())
        original = json.loads((self.bundle / "set.json").read_bytes())
        original["members"].pop()
        (self.bundle / "set.json").write_bytes(snapshot.encode_manifest(original))
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "set_digest_mismatch"):
            snapshot.verify_set(self.bundle, receipt["set_sha256"])

    def test_corrupt_last_member_blocks_every_output_before_creation(self):
        receipt = self.capture()
        path = self.saved_file("metadata")
        path.write_bytes(b"X" * path.stat().st_size)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertFalse(self.output.exists())

    def test_missing_extra_and_symlinked_members_are_not_complete_sets(self):
        receipt = self.capture()
        member_path = self.bundle / "members" / "metadata"
        held = self.root / "retained-metadata-bundle"
        member_path.rename(held)
        with self.assertRaises(snapshot.SnapshotFailure):
            snapshot.verify_set(self.bundle, receipt["set_sha256"])
        member_path.symlink_to(held, target_is_directory=True)
        with self.assertRaises(OSError):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertFalse(self.output.exists())
        member_path.rename(self.root / "retained-link")
        held.rename(member_path)
        (self.bundle / "members" / "unlisted").mkdir()
        with self.assertRaises(snapshot.SnapshotFailure):
            snapshot.verify_set(self.bundle, receipt["set_sha256"])

    def test_verification_and_recovery_enforce_aggregate_limits(self):
        receipt = self.capture()
        total = receipt["bytes"]
        for action in [lambda: snapshot.verify_set(self.bundle, receipt["set_sha256"], max_bytes=total - 1),
                       lambda: snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"], max_bytes=total - 1)]:
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "byte_limit"):
                action()
        self.assertFalse(self.output.exists())

    def test_duplicate_keys_invalid_schema_and_unsafe_names_fail_closed(self):
        self.capture()
        original = (self.bundle / "set.json").read_bytes()
        cases = [b"null", b"[]", original.replace(b'"schema":', b'"schema":"bad","schema":', 1)]
        for change in [lambda d: d.update(capture_atomic=True),
                       lambda d: d["members"][0].update(name="../escape"),
                       lambda d: d["members"].reverse(),
                       lambda d: d["members"][0].update(source_path_hex=b"relative".hex()),
                       lambda d: d["members"][0].update(manifest_sha256=None),
                       lambda d: d.update(members=[])]:
            value = json.loads(original)
            change(value)
            cases.append(snapshot.encode_manifest(value))
        for raw in cases:
            (self.bundle / "set.json").write_bytes(raw)
            with self.subTest(raw=raw[:80]), self.assertRaises(snapshot.SnapshotFailure):
                snapshot.verify_set(self.bundle, hashlib.sha256(raw).hexdigest())
        self.assertFalse((self.root / "escape").exists())

    def test_member_source_binding_is_checked_even_for_a_rehashed_set(self):
        self.capture()
        value = json.loads((self.bundle / "set.json").read_bytes())
        value["members"][0]["source_path_hex"] = os.fsencode(self.root / "other-clone").hex()
        raw = snapshot.encode_manifest(value)
        (self.bundle / "set.json").write_bytes(raw)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "source_mismatch"):
            snapshot.materialize_set(self.bundle, self.output, hashlib.sha256(raw).hexdigest())
        self.assertFalse(self.output.exists())

    def test_preflight_catches_early_payload_changes_while_reading_later_members(self):
        receipt = self.capture()
        original = snapshot.verified_bundle

        @contextlib.contextmanager
        def change_later(bundle, expected, budget):
            with original(bundle, expected, budget) as verified:
                if bundle.name == "metadata":
                    target = self.saved_file()
                    target.write_bytes(b"X" * target.stat().st_size)
                yield verified

        with mock.patch.object(snapshot, "verified_bundle", side_effect=change_later), self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
            snapshot.verify_set(self.bundle, receipt["set_sha256"])

    def test_materialization_preflight_rechecks_early_members_before_any_output(self):
        receipt = self.capture()
        original = snapshot.verified_bundle

        @contextlib.contextmanager
        def change_later(bundle, expected, budget):
            with original(bundle, expected, budget) as verified:
                if bundle.name == "metadata":
                    target = self.saved_file()
                    target.write_bytes(b"X" * target.stat().st_size)
                yield verified

        with mock.patch.object(snapshot, "verified_bundle", side_effect=change_later), self.assertRaises(snapshot.SnapshotFailure):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertFalse(self.output.exists(), "the entire preflight must finish before output creation")

    def test_late_recovery_write_failure_keeps_partial_output_without_success(self):
        receipt = self.capture()
        before = self.source_witnesses()
        original = snapshot._populate_materialized
        calls = []

        def fail(data, entries, payload, budget):
            calls.append(True)
            if len(calls) == 2:
                raise OSError("late member write failed")
            return original(data, entries, payload, budget)

        with mock.patch.object(snapshot, "_populate_materialized", side_effect=fail), self.assertRaises(OSError):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertEqual(len(calls), 2)
        self.assertEqual(tree(self.output / "data" / "database"), tree(self.database))
        self.assertFalse((self.output / "recovery.json").exists())
        self.assertEqual(self.source_witnesses(), before)
        snapshot.verify_set(self.bundle, receipt["set_sha256"])

    def test_corruption_of_earlier_recovered_member_cannot_publish_success(self):
        receipt = self.capture()
        original = snapshot._populate_materialized
        calls = 0

        def corrupt(data, entries, payload, budget):
            nonlocal calls
            original(data, entries, payload, budget)
            calls += 1
            if calls == 2:
                target = self.output / "data" / "database" / "tracker-wal-cert"
                target.write_bytes(b"X" * target.stat().st_size)

        with mock.patch.object(snapshot, "_populate_materialized", side_effect=corrupt), self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertFalse((self.output / "recovery.json").exists())

    def test_set_manifest_change_during_materialization_is_not_success(self):
        receipt = self.capture()
        original = snapshot._populate_materialized

        def change(data, entries, payload, budget):
            original(data, entries, payload, budget)
            (self.bundle / "set.json").write_bytes(b"{}")

        with mock.patch.object(snapshot, "_populate_materialized", side_effect=change), self.assertRaises(snapshot.SnapshotFailure):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertFalse((self.output / "recovery.json").exists())

    def test_materialize_never_installs_over_any_source_or_existing_destination(self):
        receipt = self.capture()
        before = self.source_witnesses()
        for destination in [self.database, self.metadata, self.database / "new", self.metadata / "new",
                            self.bundle, self.bundle / "new"]:
            with self.subTest(destination=destination), self.assertRaises((snapshot.SnapshotFailure, OSError)):
                snapshot.materialize_set(self.bundle, destination, receipt["set_sha256"])
        self.output.mkdir()
        (self.output / "keep").write_bytes(b"foreign generation")
        output_before = witness(self.output)
        with self.assertRaises(FileExistsError):
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertEqual(witness(self.output), output_before)
        self.assertEqual(self.source_witnesses(), before)

    def test_set_recovers_when_all_originals_are_gone_without_touching_new_generations(self):
        receipt = self.capture()
        expected = {name: tree(path) for name, path in self.sources}
        for name, path in self.sources:
            path.rename(self.root / ("retained-" + name))
            path.mkdir()
            (path / "new-generation").write_bytes(name.encode())
        current = self.source_witnesses()
        snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertEqual(self.source_witnesses(), current)
        for name in expected:
            self.assertEqual(tree(self.output / "data" / name), expected[name])

    def test_binary_names_empty_roots_and_private_permissions(self):
        empty = self.root / "empty"
        empty.mkdir()
        self.sources.append(("empty", empty))
        path = os.fsencode(self.database) + b"/cert-\xff\n"
        with open(path, "wb") as file:
            file.write(b"opaque non-UTF-8 certificate name")
        previous = os.umask(0)
        try:
            receipt = self.capture()
            snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        finally:
            os.umask(previous)
        self.assertEqual(tree(self.output / "data" / "database"), tree(self.database))
        self.assertEqual(tree(self.output / "data" / "empty"), {})
        for root in [self.bundle, self.output]:
            for path in [root, *root.rglob("*")]:
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)

    def test_sync_failure_refuses_without_set_completion(self):
        before = self.source_witnesses()
        with mock.patch.object(snapshot.os, "fsync", side_effect=OSError("injected sync failure")), self.assertRaises(OSError):
            self.capture()
        self.assertEqual(before, self.source_witnesses())
        self.assertFalse((self.bundle / "set.json").exists())

    def test_cli_roundtrip_and_refusal_contract(self):
        def run(*args):
            with contextlib.redirect_stdout(io.StringIO()) as output:
                status = snapshot.main(list(map(str, args)))
            return status, json.loads(output.getvalue())
        flags = ["--source", "metadata=" + str(self.metadata), "--source", "database=" + str(self.database)]
        status, refusal = run("capture-set", self.bundle, *flags)
        self.assertEqual(status, 2)
        self.assertEqual(refusal["status"], "refused_or_incomplete")
        self.assertFalse(self.bundle.exists())
        status, receipt = run("capture-set", self.bundle, *flags, "--writers-stopped")
        self.assertEqual(status, 0)
        status, _ = run("verify-set", self.bundle, "--set-sha256", receipt["set_sha256"])
        self.assertEqual(status, 0)
        status, result = run("materialize-set", self.bundle, self.output, "--set-sha256", receipt["set_sha256"])
        self.assertEqual(status, 0)
        self.assertEqual(result["status"], "materialized_set")

    def test_real_wal_only_external_database_roundtrip_and_metadata_only_negative(self):
        # Entirely separate stopped writer: opening the database in the tool
        # would be an error. Only the independent test driver uses SQLite.
        dbroot = self.root / "real-external"
        dbroot.mkdir()
        db = dbroot / "tracker"
        program = '''
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("PRAGMA journal_mode=WAL")
c.execute("PRAGMA wal_autocheckpoint=0")
c.execute("CREATE TABLE issues (id TEXT PRIMARY KEY, title TEXT)")
c.execute("CREATE TABLE events (issue_id TEXT, event TEXT)")
c.commit()
c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
c.execute("INSERT INTO issues VALUES ('bd-local', 'WAL-only local work')")
c.execute("INSERT INTO events VALUES ('bd-local', 'audit survives')")
c.commit()
os._exit(0)
'''
        result = subprocess.run([sys.executable, "-c", program, str(db)], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        (dbroot / "tracker-wal-cert").write_bytes(b"opaque certificate sentinel, not native engine validity")
        self.sources = [("metadata", self.metadata), ("database", dbroot)]
        before = self.source_witnesses()
        self.assertGreater((dbroot / "tracker-wal").stat().st_size, 32)
        self.assertNotIn(b"WAL-only local work", db.read_bytes())
        receipt = self.capture()
        snapshot.materialize_set(self.bundle, self.output, receipt["set_sha256"])
        self.assertEqual(before, self.source_witnesses())
        self.assertEqual(tree(dbroot), tree(self.output / "data" / "database"))
        self.assertFalse((self.output / "data" / "metadata" / "tracker").exists())
        main_only = self.root / "main-only.db"
        main_only.write_bytes(db.read_bytes())
        with contextlib.closing(sqlite3.connect(main_only)) as control:
            self.assertEqual(control.execute("SELECT * FROM issues").fetchall(), [])
        with contextlib.closing(sqlite3.connect(self.output / "data" / "database" / "tracker")) as recovered:
            self.assertEqual(recovered.execute("SELECT * FROM issues").fetchall(), [("bd-local", "WAL-only local work")])
            self.assertEqual(recovered.execute("SELECT * FROM events").fetchall(), [("bd-local", "audit survives")])
            self.assertEqual(recovered.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
        snapshot.verify_set(self.bundle, receipt["set_sha256"])


if __name__ == "__main__":
    unittest.main()
