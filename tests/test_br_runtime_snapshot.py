"""Offline preservation tests; every source is a new, retained temporary fixture."""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "br_runtime_snapshot.py"
SPEC = importlib.util.spec_from_file_location("snapshot", SCRIPT)
snapshot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snapshot)


@unittest.skipUnless(os.name == "posix", "descriptor-relative POSIX implementation")
class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-runtime-snapshot-tests-"))
        self.repo = self.root / "repo"
        self.repo.mkdir()
        (self.repo / ".git").mkdir()
        self.source = self.repo / ".beads"
        self.source.mkdir()
        self.bundle = self.root / "saved"
        for name, data in {
            "beads.db": b"main database\0",
            "beads.db-wal": b"committed WAL-only work\xff",
            "beads.db-wal-cert": b"clone-specific certificate",
            "beads.db-wal-cert-head": b"clone-specific certificate head",
            "beads.db.fsqlite-migration-state": b"migration evidence",
            "beads.db-fsqlite-ns-use": b"namespace evidence",
            "issues.jsonl": b'{"id":"bd-local"}\n',
            "unknown.future-sidecar": b"preserve without recognizing the suffix",
        }.items():
            (self.source / name).write_bytes(data)
        (self.source / ".br_recovery" / "incident" / "empty").mkdir(parents=True)
        (self.source / ".br_recovery" / "incident" / "receipt.json").write_bytes(b"receipt")

    def capture(self, **kwargs):
        return snapshot.capture(self.source, self.bundle, writers_stopped=True, **kwargs)

    def witness(self):
        return {os.fsencode(path.relative_to(self.source)):
                (snapshot.identity(path.stat()), path.read_bytes())
                for path in self.source.rglob("*") if path.is_file()}

    def manifest(self):
        return json.loads((self.bundle / "manifest.json").read_bytes())

    def rewrite_manifest(self, value):
        raw = snapshot.encode_manifest(value)
        (self.bundle / "manifest.json").write_bytes(raw)
        return hashlib.sha256(raw).hexdigest()

    def test_capture_requires_quiescence_assertion_before_writing(self):
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "stop_all"):
            snapshot.capture(self.source, self.bundle)
        self.assertFalse(self.bundle.exists())

    def test_capture_and_verify_whole_directory_including_unknown_files_and_empty_dirs(self):
        before = self.witness()
        receipt = self.capture()
        self.assertEqual(self.witness(), before)
        verified = snapshot.verify(self.bundle, receipt["manifest_sha256"])
        self.assertEqual(verified["files"], 9)
        self.assertFalse(verified["engine_validated"])
        entries = self.manifest()["entries"]
        paths = {bytes.fromhex(record["path_hex"]) for record in entries}
        self.assertIn(b".br_recovery/incident/empty", paths)
        self.assertIn(b"unknown.future-sidecar", paths)
        self.assertNotIn(b"beads.db-journal", paths)
        for record in entries:
            if record["kind"] == "file":
                live = self.source / os.fsdecode(bytes.fromhex(record["path_hex"]))
                self.assertEqual((self.bundle / "payload" / record["object"]).read_bytes(), live.read_bytes())

    def test_capture_permissions_are_private_even_with_open_umask(self):
        previous = os.umask(0)
        try:
            self.capture()
        finally:
            os.umask(previous)
        self.assertEqual(stat.S_IMODE(self.bundle.stat().st_mode), 0o700)
        for path in self.bundle.rglob("*"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)

    def test_non_utf8_and_control_character_names_are_lossless(self):
        name = b"certificate-\xff\n\t"
        with open(os.fsencode(self.source) + b"/" + name, "wb") as file:
            file.write(b"exact bytes")
        receipt = self.capture()
        self.assertIn(name.hex(), {record["path_hex"] for record in self.manifest()["entries"]})
        snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_existing_output_is_never_reused(self):
        self.bundle.mkdir()
        sentinel = self.bundle / "keep"
        sentinel.write_bytes(b"retain")
        with self.assertRaises(FileExistsError):
            self.capture()
        self.assertEqual(sentinel.read_bytes(), b"retain")
        self.assertEqual(set(self.bundle.iterdir()), {sentinel})

    def test_output_inside_worktree_or_overlapping_source_is_refused(self):
        for target in [self.repo / "saved", self.source / "saved", self.source, self.repo]:
            with self.subTest(target=target), self.assertRaises((snapshot.SnapshotFailure, FileExistsError)):
                snapshot.capture(self.source, target, writers_stopped=True)
        self.assertFalse((self.repo / "saved").exists())

    def test_gitfile_worktree_marker_is_also_refused(self):
        linked = self.root / "linked"
        linked.mkdir()
        (linked / ".git").write_text("gitdir: /not/followed")
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "worktree"):
            snapshot.capture(self.source, linked / "saved", writers_stopped=True)

    def test_output_ancestor_symlink_is_not_followed(self):
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(OSError):
            snapshot.capture(self.source, alias / "saved", writers_stopped=True)
        self.assertFalse(self.bundle.exists())

    def test_maximum_depth_snapshot_is_also_verifiable(self):
        source = self.root / "depth"
        source.mkdir()
        (source / "empty").mkdir()
        (source / "file").write_bytes(b"at the maximum depth")
        with mock.patch.object(snapshot, "MAX_DEPTH", 1):
            receipt = snapshot.capture(source, self.bundle, writers_stopped=True)
            snapshot.verify(self.bundle, receipt["manifest_sha256"])
            (source / "empty" / "too-deep").write_bytes(b"refuse")
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_limit"):
                snapshot.capture(source, self.root / "deep-output", writers_stopped=True)

    def test_source_root_and_ancestor_symlinks_are_not_followed(self):
        alias = self.root / "alias"
        alias.symlink_to(self.repo, target_is_directory=True)
        leaf = self.root / "leaf"
        leaf.symlink_to(self.source, target_is_directory=True)
        for path in [alias / ".beads", leaf]:
            with self.subTest(path=path), self.assertRaises(OSError):
                snapshot.capture(path, self.bundle, writers_stopped=True)
        self.assertFalse(self.bundle.exists())

    def test_source_symlinks_hardlinks_and_fifos_are_refused_without_output(self):
        factories = [lambda path: path.symlink_to(self.root / "missing"),
                     lambda path: os.link(self.source / "beads.db", path),
                     os.mkfifo]
        for index, factory in enumerate(factories):
            source = self.root / f"unsafe-{index}"
            source.mkdir()
            factory(source / "unsafe")
            with self.subTest(index=index), self.assertRaisesRegex(snapshot.SnapshotFailure, "refused"):
                snapshot.capture(source, self.root / f"output-{index}", writers_stopped=True)
            self.assertFalse((self.root / f"output-{index}").exists())

    def test_git_metadata_inside_source_is_refused(self):
        (self.source / ".git").write_bytes(b"do not copy git data")
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "git_metadata"):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_byte_entry_and_depth_limits_refuse_instead_of_truncating(self):
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "byte_limit"):
            self.capture(max_bytes=1)
        with mock.patch.object(snapshot, "MAX_ENTRIES", 2), self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_limit"):
            self.capture()
        with mock.patch.object(snapshot, "MAX_DEPTH", 1), self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_limit"):
            self.capture()
        self.assertFalse(self.bundle.exists())

    def test_deadline_and_invalid_budget_refuse_before_output(self):
        with mock.patch.object(snapshot.time, "monotonic", side_effect=[0, 2]), self.assertRaisesRegex(snapshot.SnapshotFailure, "deadline"):
            self.capture(timeout=1)
        for value in [float("nan"), float("inf"), -1, 0]:
            with self.subTest(value=value), self.assertRaises(snapshot.SnapshotFailure):
                self.capture(timeout=value)
        self.assertFalse(self.bundle.exists())

    def test_source_mutation_after_copy_leaves_no_completion_manifest(self):
        original = snapshot.stream
        changed = False

        def mutate(file, size, budget, destination=None):
            nonlocal changed
            result = original(file, size, budget, destination)
            if destination is not None and not changed:
                changed = True
                (self.source / "beads.db-wal-cert").write_bytes(b"concurrent certificate generation")
            return result

        with mock.patch.object(snapshot, "stream", side_effect=mutate), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertTrue(changed)
        self.assertTrue(self.bundle.exists())
        self.assertFalse((self.bundle / "manifest.json").exists())

    def test_source_addition_after_copy_is_not_silently_omitted(self):
        original = snapshot.stream

        def add(file, size, budget, destination=None):
            result = original(file, size, budget, destination)
            if destination is not None:
                (self.source / "appeared-wal-cert").write_bytes(b"new")
            return result

        with mock.patch.object(snapshot, "stream", side_effect=add), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertFalse((self.bundle / "manifest.json").exists())

    def test_source_root_replacement_is_detected(self):
        original = snapshot.stream
        swapped = False

        def swap(file, size, budget, destination=None):
            nonlocal swapped
            result = original(file, size, budget, destination)
            if destination is not None and not swapped:
                swapped = True
                self.source.rename(self.repo / "retained-original")
                self.source.mkdir()
            return result

        with mock.patch.object(snapshot, "stream", side_effect=swap), self.assertRaises(snapshot.SnapshotFailure):
            self.capture()
        self.assertFalse((self.bundle / "manifest.json").exists())
        self.assertTrue((self.repo / "retained-original" / "beads.db").exists())

    def test_fsync_failure_leaves_incomplete_output_and_source_intact(self):
        before = self.witness()
        with mock.patch.object(snapshot.os, "fsync", side_effect=OSError("injected fsync failure")), self.assertRaises(OSError):
            self.capture()
        self.assertEqual(before, self.witness())
        self.assertFalse((self.bundle / "manifest.json").exists())

    def test_payload_corruption_and_wrong_trusted_receipt_are_rejected(self):
        receipt = self.capture()
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "manifest_digest"):
            snapshot.verify(self.bundle, "0" * 64)
        record = next(record for record in self.manifest()["entries"] if record["kind"] == "file")
        target = self.bundle / "payload" / record["object"]
        target.write_bytes(b"x" * record["size"])
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "payload_digest"):
            snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_missing_extra_and_linked_payload_members_fail_verification(self):
        receipt = self.capture()
        record = next(record for record in self.manifest()["entries"] if record["kind"] == "file")
        target = self.bundle / "payload" / record["object"]
        held = self.root / "held-object"
        target.rename(held)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory"):
            snapshot.verify(self.bundle, receipt["manifest_sha256"])
        target.symlink_to(held)
        with self.assertRaises(OSError):
            snapshot.verify(self.bundle, receipt["manifest_sha256"])
        target.rename(self.root / "held-link")
        os.link(held, target)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "identity"):
            snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_extra_payload_cannot_be_mistaken_for_a_complete_bundle(self):
        receipt = self.capture()
        (self.bundle / "payload" / "unlisted").write_bytes(b"extra")
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory"):
            snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_even_a_rehashed_manifest_cannot_inject_paths_or_objects(self):
        self.capture()
        original = self.manifest()
        for bad_path in [b"../escape", b"/escape", b"a/../../escape", b".git/index", b"a\0b"]:
            value = json.loads(json.dumps(original))
            value["entries"][1]["path_hex"] = bad_path.hex()
            expected = self.rewrite_manifest(value)
            with self.subTest(path=bad_path), self.assertRaises(snapshot.SnapshotFailure):
                snapshot.verify(self.bundle, expected)
        value = json.loads(json.dumps(original))
        next(record for record in value["entries"] if record["kind"] == "file")["object"] = "../escape"
        with self.assertRaises(snapshot.SnapshotFailure):
            snapshot.verify(self.bundle, self.rewrite_manifest(value))
        self.assertFalse((self.root / "escape").exists())

    def test_duplicate_manifest_members_and_invalid_sizes_are_rejected(self):
        self.capture()
        value = self.manifest()
        raw = (self.bundle / "manifest.json").read_bytes()
        duplicate = raw.replace(b'"schema":', b'"schema":"untrusted","schema":', 1)
        (self.bundle / "manifest.json").write_bytes(duplicate)
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "duplicate"):
            snapshot.verify(self.bundle, hashlib.sha256(duplicate).hexdigest())
        for bad in [True, -1, "1", 1.0]:
            modified = json.loads(json.dumps(value))
            next(record for record in modified["entries"] if record["kind"] == "file")["size"] = bad
            with self.subTest(size=bad), self.assertRaises(snapshot.SnapshotFailure):
                snapshot.verify(self.bundle, self.rewrite_manifest(modified))

    def test_cli_emits_receipt_and_never_says_an_error_is_success(self):
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            code = snapshot.main(["capture", str(self.source), str(self.bundle)])
        self.assertEqual(code, 2)
        self.assertEqual(json.loads(stdout.getvalue())["status"], "refused_or_incomplete")
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            code = snapshot.main(["capture", str(self.source), str(self.bundle), "--writers-stopped"])
        self.assertEqual(code, 0)
        expected = json.loads(stdout.getvalue())["manifest_sha256"]
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            code = snapshot.main(["verify", str(self.bundle), "--manifest-sha256", expected])
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(stdout.getvalue())["status"], "verified")


if __name__ == "__main__":
    unittest.main()
