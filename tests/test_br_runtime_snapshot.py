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
import sqlite3
import sys
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

    def test_double_slash_alias_cannot_create_a_bundle_inside_its_source(self):
        source = self.root / "offline"
        source.mkdir()
        (source / "beads.db").write_bytes(b"keep")
        alias = Path("/" + str(source))
        target = source / "saved"
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "double_slash"):
            snapshot.capture(alias, target, writers_stopped=True)
        self.assertFalse(target.exists())

    def test_directory_enumeration_stops_at_the_entry_bound(self):
        with snapshot.directory(self.source) as fd:
            with self.assertRaisesRegex(snapshot.SnapshotFailure, "inventory_limit"):
                snapshot.directory_names(fd, snapshot.Budget(10, 1024), 1)

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

    def test_materialize_preserves_every_byte_empty_directory_and_absence(self):
        name = b"local-\xff\n"
        with open(os.fsencode(self.source) + b"/" + name, "wb") as file:
            file.write(b"unusual name, exact payload")
        before = self.witness()
        receipt = self.capture()
        destination = self.root / "recovered"
        result = snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertEqual(result["status"], "materialized")
        self.assertFalse(result["live_installation_performed"])
        self.assertFalse(result["engine_validated"])
        self.assertEqual(before, self.witness())
        data = destination / "data"
        for path, (_info, contents) in before.items():
            self.assertEqual((data / os.fsdecode(path)).read_bytes(), contents)
        self.assertTrue((data / ".br_recovery" / "incident" / "empty").is_dir())
        self.assertFalse((data / "beads.db-journal").exists())
        for path in destination.rglob("*"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)
        self.assertEqual(json.loads((destination / "recovery.json").read_bytes()), result)
        snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_materialize_validates_corruption_before_creating_output(self):
        receipt = self.capture()
        record = next(record for record in self.manifest()["entries"] if record["kind"] == "file")
        (self.bundle / "payload" / record["object"]).write_bytes(b"x" * record["size"])
        destination = self.root / "recovered"
        with self.assertRaisesRegex(snapshot.SnapshotFailure, "digest"):
            snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertFalse(destination.exists())

    def test_materialize_never_reuses_existing_output_or_live_source(self):
        receipt = self.capture()
        destination = self.root / "recovered"
        destination.mkdir()
        sentinel = destination / "beads.db-wal-cert"
        sentinel.write_bytes(b"foreign generation")
        for target in [destination, self.source, self.source / "new", self.bundle / "new"]:
            with self.subTest(target=target), self.assertRaises((snapshot.SnapshotFailure, FileExistsError)):
                snapshot.materialize(self.bundle, target, receipt["manifest_sha256"])
        self.assertEqual(sentinel.read_bytes(), b"foreign generation")
        self.assertFalse((self.source / "new").exists())

    def test_materialize_does_not_touch_a_new_live_generation(self):
        before = self.witness()
        receipt = self.capture()
        self.source.rename(self.repo / "retained-old")
        self.source.mkdir()
        sentinel = self.source / "beads.db-wal-cert"
        sentinel.write_bytes(b"new live generation must remain separate")
        current = self.witness()
        destination = self.root / "recovered"
        snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertEqual(current, self.witness())
        self.assertEqual((destination / "data" / "beads.db-wal-cert").read_bytes(), before[b"beads.db-wal-cert"][1])

    def test_interrupted_materialization_retains_partial_output_without_receipt(self):
        receipt = self.capture()
        original = snapshot.stream
        destination = self.root / "recovered"
        writes = 0

        def fail(file, size, budget, target=None):
            nonlocal writes
            if target is not None:
                writes += 1
                if writes == 2:
                    raise OSError("injected recovery write failure")
            return original(file, size, budget, target)

        with mock.patch.object(snapshot, "stream", side_effect=fail), self.assertRaises(OSError):
            snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertTrue((destination / "data").exists())
        self.assertFalse((destination / "recovery.json").exists())
        snapshot.verify(self.bundle, receipt["manifest_sha256"])

    def test_materialization_readback_detects_bad_destination_bytes(self):
        receipt = self.capture()
        original = snapshot.stream
        destination = self.root / "recovered"

        def corrupt(file, size, budget, target=None):
            result = original(file, size, budget, target)
            if target is not None and size:
                os.pwrite(target, b"X", 0)
            return result

        with mock.patch.object(snapshot, "stream", side_effect=corrupt), self.assertRaisesRegex(snapshot.SnapshotFailure, "materialized_digest"):
            snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertFalse((destination / "recovery.json").exists())

    def test_bundle_change_after_preflight_cannot_publish_recovery_receipt(self):
        receipt = self.capture()
        original = snapshot.stream
        destination = self.root / "recovered"

        def add(file, size, budget, target=None):
            result = original(file, size, budget, target)
            if target is not None:
                (self.bundle / "unexpected").write_bytes(b"bundle changed")
            return result

        with mock.patch.object(snapshot, "stream", side_effect=add), self.assertRaisesRegex(snapshot.SnapshotFailure, "bundle_changed"):
            snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertFalse((destination / "recovery.json").exists())

    def test_materialization_failure_never_marks_partial_output_complete(self):
        receipt = self.capture()
        destination = self.root / "recovered"
        with mock.patch.object(snapshot.os, "fsync", side_effect=OSError("sync failed")), self.assertRaises(OSError):
            snapshot.materialize(self.bundle, destination, receipt["manifest_sha256"])
        self.assertFalse((destination / "recovery.json").exists())

    def test_empty_directory_can_be_captured_and_materialized(self):
        source = self.root / "empty"
        source.mkdir()
        receipt = snapshot.capture(source, self.bundle, writers_stopped=True, max_bytes=0)
        result = snapshot.materialize(self.bundle, self.root / "recovered", receipt["manifest_sha256"], max_bytes=0)
        self.assertEqual(result["files"], 0)
        self.assertEqual(list(Path(result["data_directory"]).iterdir()), [])

    def git(self, directory, *args, success=True):
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
        env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                   GIT_TERMINAL_PROMPT="0", LC_ALL="C")
        result = subprocess.run(["git", "-c", f"core.hooksPath={os.devnull}",
                                 "-c", "core.fsmonitor=false", "-c", "user.name=Fixture",
                                 "-c", "user.email=fixture@localhost", *args],
                                cwd=directory, env=env, capture_output=True, timeout=10)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        return result

    def sqlite_writer(self, path, sentinel, initialize=False):
        # A separate process deliberately leaves a committed WAL without close's
        # checkpoint. It has exited before capture: there is no active writer.
        program = '''
import os, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("PRAGMA journal_mode=WAL")
c.execute("PRAGMA wal_autocheckpoint=0")
if sys.argv[3] == "yes":
    c.execute("CREATE TABLE issues (title TEXT)")
    c.commit()
    c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
c.execute("INSERT INTO issues VALUES (?)", (sys.argv[2],))
c.commit()
os._exit(0)
'''
        result = subprocess.run([sys.executable, "-c", program, str(path), sentinel,
                                 "yes" if initialize else "no"], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))

    def snapshot_cli(self, *args):
        result = subprocess.run([sys.executable, str(SCRIPT), *map(str, args)],
                                capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout.decode(errors="replace") + result.stderr.decode(errors="replace"))
        return json.loads(result.stdout)

    def two_clone_recovery(self, dirty):
        author = self.root / "author"
        author.mkdir()
        self.git(author, "init", "-b", "main")
        tracker = author / ".beads"
        tracker.mkdir()
        seed = "seed committed only in WAL"
        self.sqlite_writer(tracker / "beads.db", seed, initialize=True)
        self.assertNotIn(seed.encode(), (tracker / "beads.db").read_bytes())
        self.assertIn(seed.encode(), (tracker / "beads.db-wal").read_bytes())
        # These are opaque preservation sentinels, NOT FrankenSQLite certificates.
        (tracker / "beads.db-wal-cert").write_bytes(b"author certificate sentinel")
        (tracker / "beads.db-wal-cert-head").write_bytes(b"author certificate-head sentinel")
        (tracker / "issues.jsonl").write_bytes(b"{\"shared\":true}\n")
        self.git(author, "add", ".beads")
        self.git(author, "commit", "-m", "synthetic tracked runtime")
        peer = self.root / "peer"
        self.git(self.root, "clone", "--no-local", str(author), str(peer))
        local = peer / ".beads"
        local_seed = "peer-only committed work must not be replaced by author data"
        if dirty:
            self.sqlite_writer(local / "beads.db", local_seed)
            (local / "beads.db-wal-cert").write_bytes(b"peer-specific certificate sentinel")
        local_before = {path.name: path.read_bytes() for path in local.iterdir()}
        index_before = (peer / ".git" / "index").read_bytes()
        head_before = self.git(peer, "rev-parse", "HEAD").stdout
        saved = self.root / "peer-backup"
        receipt = self.snapshot_cli("capture", local, saved, "--writers-stopped")
        self.assertEqual(index_before, (peer / ".git" / "index").read_bytes())
        self.assertEqual(head_before, self.git(peer, "rev-parse", "HEAD").stdout)
        self.assertEqual(local_before, {path.name: path.read_bytes() for path in local.iterdir()})
        runtime = sorted(".beads/" + name for name in local_before if name != "issues.jsonl")
        # Only this disposable fixture driver mutates Git, never the tool.
        self.git(author, "rm", "--cached", "--", *runtime)
        (tracker / ".gitignore").write_bytes(b"beads.db*\n")
        self.git(author, "add", ".beads/.gitignore")
        self.git(author, "commit", "-m", "synthetic runtime untracking")
        self.git(peer, "fetch", "origin")
        attempt = self.git(peer, "merge", "--ff-only", "origin/main", success=False)
        if dirty:
            self.assertNotEqual(attempt.returncode, 0, "dirty tracked WAL must obstruct the naive update")
            self.assertEqual(local_before, {path.name: path.read_bytes() for path in local.iterdir()})
            # Model an operator retaining the stopped original before updating.
            # No reset/clean or destruction of the peer generation is used.
            local.rename(self.root / "peer-original-retained")
            self.git(peer, "merge", "--ff-only", "origin/main")
        else:
            self.assertEqual(attempt.returncode, 0, attempt.stderr.decode(errors="replace"))
        self.assertFalse((local / "beads.db").exists(), "Git actually removes the tracked peer database")
        self.assertFalse((local / "beads.db-wal-cert").exists())
        self.snapshot_cli("verify", saved, "--manifest-sha256", receipt["manifest_sha256"])
        restored = self.snapshot_cli("materialize", saved, self.root / "peer-recovered",
                                     "--manifest-sha256", receipt["manifest_sha256"])
        data = Path(restored["data_directory"])
        self.assertEqual(local_before, {path.name: path.read_bytes() for path in data.iterdir()})
        # Negative control: copying only main loses committed WAL-only rows.
        main_only = self.root / "main-only.db"
        main_only.write_bytes(local_before["beads.db"])
        with sqlite3.connect(main_only) as control:
            self.assertNotIn((seed,), control.execute("SELECT title FROM issues").fetchall())
        with sqlite3.connect(data / "beads.db") as recovered:
            rows = recovered.execute("SELECT title FROM issues ORDER BY title").fetchall()
            self.assertEqual(rows, sorted([(seed,), (local_seed,)] if dirty else [(seed,)]))
            self.assertEqual(recovered.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
        # Engine reads of the NEW recovery directory cannot change the bundle.
        self.snapshot_cli("verify", saved, "--manifest-sha256", receipt["manifest_sha256"])

    def test_two_clone_untracking_recovers_clean_peer_with_real_wal_only_work(self):
        self.two_clone_recovery(False)

    def test_two_clone_untracking_recovers_dirty_peer_without_mixing_generations(self):
        self.two_clone_recovery(True)


if __name__ == "__main__":
    unittest.main()
