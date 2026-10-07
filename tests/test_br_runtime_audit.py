"""Real-Git tests for the explicitly invoked runtime-file auditor.

Every repository is newly created under a dedicated temporary fixture root.
No user's checkout or tracker is opened or modified. Fixture directories are
retained for inspection rather than cleaned with a destructive shell command.
"""
from __future__ import annotations

import contextlib
import argparse
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "br_runtime_audit.py"
spec = importlib.util.spec_from_file_location("br_runtime_audit", SCRIPT)
audit = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = audit
spec.loader.exec_module(audit)


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="br-runtime-audit-test-"))
        self.repo = self.base / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Audit test")
        self.git("config", "user.email", "audit@example.invalid")

    def git(self, *args, root=None, check=True):
        result = subprocess.run(["git", "-C", str(root or self.repo), *args],
                                env=audit.hardened_environment(), capture_output=True, check=False)
        if check and result.returncode:
            self.fail(f"fixture git command failed: {args!r}: {result.stderr!r}")
        return result

    def write(self, name, data=b"fixture", root=None):
        path = (root or self.repo) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def commit(self, *paths):
        self.git("add", "-f", "--", *paths)
        self.git("commit", "-qm", "synthetic fixture")
        return self.git("rev-parse", "HEAD").stdout.strip().decode()

    def run_audit(self, **kwargs):
        return audit.audit_repository(self.repo, **kwargs)

    def test_unborn_empty_repository_is_complete_and_clean(self):
        report = self.run_audit()
        self.assertEqual(report["status"], "clean", report)
        self.assertTrue(report["complete"])
        self.assertIsNone(report["head"])

    def test_unborn_index_is_not_mistaken_for_no_tracking(self):
        self.write(".beads/beads.db-wal-cert")
        self.git("add", "-f", ".beads/beads.db-wal-cert")
        report = self.run_audit()
        self.assertEqual(report["status"], "unsafe", report)
        self.assertEqual(len(report["findings"]), 1)
        self.assertFalse(report["findings"][0]["head_entries"])

    def test_legitimate_shared_files_are_not_runtime(self):
        for name in ("issues.jsonl", "metadata.json", "config.yaml", "policy.yaml", "README.md", ".gitignore", "custom-export.jsonl"):
            self.write(".beads/" + name)
        self.commit(".beads")
        self.assertEqual(self.run_audit()["status"], "clean")

    def test_all_major_runtime_families_are_detected_even_when_ignored(self):
        paths = {"beads.db": "database", "beads.db-wal": "wal", "beads.db-shm": "shared_index",
                 "beads.db-journal": "journal", "beads.db-wal-cert": "certificate",
                 "beads.db-wal-cert-head": "certificate_head", "beads.db-fsqlite-ns-gate": "namespace",
                 "beads.db-fsqlite-ns-use": "namespace", "beads.db.fsqlite-migration-state": "migration_state",
                 ".br-db-openers-abc.lock": "coordination_lock", ".br-wal-index-original": "shared_index_recovery",
                 ".br_recovery/run/original": "recovery", ".br_history/issues.jsonl": "local_history",
                 ".write-waiters.lock/owner": "coordination_lock", "sync_base.jsonl": "local_state",
                 "beads.left.jsonl": "merge_temporary", ".beads.vacuum.1.tmp": "temporary"}
        for name in paths:
            self.write(".beads/" + name)
        self.write(".beads/.gitignore", b"*\n")
        self.commit(".beads")
        report = self.run_audit()
        self.assertEqual({f["path"][len(".beads/"):]: f["kind"] for f in report["findings"]}, paths)

    def test_staged_untracking_remains_unsafe_until_head_changes(self):
        path = ".beads/beads.db-wal-cert"
        original = self.write(path, b"machine-local certificate")
        self.commit(path)
        self.git("update-index", "--force-remove", "--", path)
        finding = self.run_audit()["findings"][0]
        self.assertTrue(finding["staged_untracking"])
        self.assertTrue(finding["head_entries"])
        self.assertFalse(finding["index_entries"])
        self.assertEqual(original.read_bytes(), b"machine-local certificate")

    def test_incoming_untracking_warns_before_another_clone_deletes_its_copy(self):
        path = ".beads/beads.db-wal-cert"
        self.write(path, b"certificate in old Git commit")
        self.commit(path)
        peer = self.base / "peer"
        self.git("clone", "-q", str(self.repo), str(peer))
        self.git("update-index", "--force-remove", "--", path)
        self.git("commit", "-qm", "remove runtime from future index")
        self.git("fetch", "-q", "origin", "main", root=peer)
        before = (peer / path).read_bytes()
        report = audit.audit_repository(peer, incoming_ref="origin/main")
        self.assertEqual(report["status"], "unsafe", report)
        self.assertTrue(report["findings"][0]["incoming_deletes"])
        self.assertEqual((peer / path).read_bytes(), before)
        # This is deliberately outside the auditor, in a disposable peer: it
        # proves the cross-clone deletion risk that a local --cached removal hides.
        self.git("merge", "--ff-only", "origin/main", root=peer)
        self.assertFalse((peer / path).exists())

    def test_incoming_replacement_does_not_read_or_modify_local_runtime_bytes(self):
        name = ".beads/beads.db-wal-cert"
        self.write(name, b"old")
        old = self.commit(name)
        self.write(name, b"different upstream certificate")
        new = self.commit(name)
        peer = self.base / "peer"
        self.git("clone", "-q", str(self.repo), str(peer))
        self.git("checkout", "-q", "--detach", old, root=peer)
        self.write(name, b"my independent local certificate", root=peer)
        report = audit.audit_repository(peer, incoming_ref=new)
        self.assertTrue(report["findings"][0]["incoming_replaces"])
        self.assertEqual((peer / name).read_bytes(), b"my independent local certificate")

    def test_incoming_additions_are_reported(self):
        self.write(".beads/issues.jsonl")
        old = self.commit(".beads/issues.jsonl")
        self.write(".beads/beads.db")
        new = self.commit(".beads/beads.db")
        self.git("checkout", "-q", "--detach", old)
        self.assertTrue(self.run_audit(incoming_ref=new)["findings"][0]["incoming_adds"])

    def test_extensionless_database_outside_beads_is_explicitly_scoped(self):
        self.write("cache/tracker")
        self.write("cache/tracker-wal-cert")
        self.write("cache/unrelated.db")
        self.commit("cache")
        report = self.run_audit(databases=("cache/tracker",))
        self.assertEqual([f["path"] for f in report["findings"]], ["cache/tracker", "cache/tracker-wal-cert"])

    def test_nested_custom_tracker_and_literal_pathspec_characters(self):
        self.write("track[1]/.beads/state.sqlite3-wal")
        self.write("track1/.beads/state.sqlite3-wal")
        self.commit("track[1]", "track1")
        report = self.run_audit(beads_dirs=("track[1]/.beads",))
        self.assertEqual([f["path"] for f in report["findings"]], ["track[1]/.beads/state.sqlite3-wal"])

    def test_scope_does_not_match_neighbor_directory(self):
        self.write(".beads-archive/beads.db")
        self.commit(".beads-archive")
        self.assertEqual(self.run_audit()["status"], "clean")

    def test_untracked_runtime_is_not_a_git_hazard(self):
        self.write(".beads/beads.db-wal-cert")
        self.assertEqual(self.run_audit()["status"], "clean")

    def test_retained_wal_suffixes_match_initialization_ignore_policy(self):
        self.write(".beads/beads.db-wal-cert.retained")
        self.commit(".beads")
        self.assertEqual(self.run_audit()["findings"][0]["kind"], "wal_sidecar")

    def test_quarantine_directory_contents_are_runtime_not_shared_json(self):
        paths = {
            ".br-wal-index-incident/receipt.json": "shared_index_recovery",
            ".br-wal-index-incident/retained/original": "shared_index_recovery",
            "nested/.br_recovery/run/receipt.json": "recovery",
            "nested/.br_history/old-export.jsonl": "local_history",
            "nested/.write-waiters.lock/owner.json": "coordination_lock",
        }
        for name in paths:
            self.write(".beads/" + name)
        self.commit(".beads")
        report = self.run_audit()
        self.assertEqual(
            {f["path"][len(".beads/"):]: f["kind"] for f in report["findings"]}, paths)

    def test_replacement_refs_cannot_hide_head_runtime_entries(self):
        name = ".beads/beads.db-wal-cert"
        self.write(name)
        original = self.commit(name)
        self.git("update-index", "--force-remove", "--", name)
        self.git("commit", "-qm", "clean synthetic replacement")
        replacement = self.git("rev-parse", "HEAD").stdout.strip().decode()
        self.git("replace", original, replacement)
        self.git("update-ref", "refs/heads/main", original, replacement)
        report = self.run_audit()
        self.assertEqual(report["status"], "unsafe", report)
        self.assertEqual(report["head"], original)
        self.assertTrue(report["findings"][0]["staged_untracking"])

    def test_deep_tracker_scope_cannot_hide_a_gitlink_ancestor(self):
        self.write("README.md")
        commit = self.commit("README.md")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{commit},nested")
        report = self.run_audit(beads_dirs=("nested/tracker/.beads",))
        self.assertFalse(report["complete"], report)
        self.assertEqual(report["reason"], "scope_crosses_tracked_non_directory")

    @unittest.skipUnless(os.name == "posix", "requires symlinks")
    def test_deep_database_scope_cannot_hide_a_symlink_ancestor(self):
        outside = self.base / "other-tracker"
        outside.mkdir()
        (self.repo / "cache").symlink_to(outside, target_is_directory=True)
        self.commit("cache")
        report = self.run_audit(databases=("cache/nested/tracker",))
        self.assertFalse(report["complete"], report)
        self.assertEqual(report["reason"], "scope_crosses_tracked_non_directory")

    @unittest.skipUnless(os.name == "posix", "byte-preserving filenames require POSIX")
    def test_non_utf8_newline_tab_and_escape_names_are_lossless(self):
        for name in (".beads/line\nname.db", ".beads/tab\tname.db", ".beads/\x1b[31m.db", os.fsdecode(b".beads/invalid-\xff.db")):
            self.write(name)
        self.commit(".beads")
        report = self.run_audit()
        self.assertEqual(len(report["findings"]), 4)
        for finding in report["findings"]:
            self.assertEqual(bytes.fromhex(finding["path_bytes_hex"]), os.fsencode(finding["path"]))
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(audit.main([str(self.repo)]), 1)
        self.assertNotIn("\x1b", output.getvalue())
        self.assertIn("\\nname.db", output.getvalue())

    @unittest.skipUnless(os.name == "posix", "requires symlinks")
    def test_runtime_symlink_is_detected_without_following_it(self):
        (self.repo / ".beads").mkdir()
        target = self.base / "private"
        target.write_bytes(b"must not be opened")
        (self.repo / ".beads/beads.db").symlink_to(target)
        self.commit(".beads")
        finding = self.run_audit()["findings"][0]
        self.assertEqual(finding["index_entries"][0]["mode"], "120000")
        self.assertEqual(target.read_bytes(), b"must not be opened")

    @unittest.skipUnless(os.name == "posix", "requires symlinks")
    def test_symlinked_tracker_scope_cannot_masquerade_as_clean(self):
        outside = self.base / "other-tracker"
        outside.mkdir()
        (self.repo / ".beads").symlink_to(outside, target_is_directory=True)
        self.commit(".beads")
        report = self.run_audit()
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "scope_crosses_tracked_non_directory")

    def test_explicit_database_inside_gitlink_requires_separate_repository_audit(self):
        self.write("README.md")
        commit = self.commit("README.md")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{commit},cache")
        report = self.run_audit(databases=("cache/tracker",))
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "scope_crosses_tracked_non_directory")

    def test_unmerged_stages_are_preserved(self):
        name = ".beads/beads.db-wal"
        self.write(name)
        self.commit(name)
        blob = self.git("rev-parse", f"HEAD:{name}").stdout.strip()
        entries = b"".join(b"100644 " + blob + b" " + str(stage).encode() + b"\t" + name.encode() + b"\0" for stage in (1, 2, 3))
        self.git("update-index", "--force-remove", "--", name)
        process = subprocess.run(["git", "-C", str(self.repo), "update-index", "-z", "--index-info"],
                                 input=entries, env=audit.hardened_environment(), capture_output=True)
        self.assertEqual(process.returncode, 0, process.stderr)
        finding = self.run_audit()["findings"][0]
        self.assertTrue(finding["unmerged"])
        self.assertEqual([entry["stage"] for entry in finding["index_entries"]], [1, 2, 3])

    def test_sha256_git_repository(self):
        other = self.base / "sha256"
        other.mkdir()
        result = self.git("init", "-q", "-b", "main", "--object-format=sha256", root=other, check=False)
        if result.returncode:
            self.skipTest("Git does not support SHA-256 repositories")
        self.write(".beads/beads.db", root=other)
        self.git("add", ".beads", root=other)
        report = audit.audit_repository(other)
        self.assertEqual(report["status"], "unsafe", report)
        self.assertEqual(len(report["findings"][0]["index_entries"][0]["object_id"]), 64)

    def test_linked_worktree_is_supported(self):
        self.write(".beads/beads.db")
        self.commit(".beads")
        other = self.base / "linked"
        self.git("worktree", "add", "-q", "-b", "linked", str(other))
        report = audit.audit_repository(other)
        self.assertEqual(report["status"], "unsafe", report)

    def test_non_repository_and_nested_directory_are_not_clean(self):
        outside = self.base / "outside"
        outside.mkdir()
        self.assertFalse(audit.audit_repository(outside)["complete"])
        nested = self.repo / "nested"
        nested.mkdir()
        report = audit.audit_repository(nested)
        self.assertEqual(report["reason"], "repository_root_required")
        self.assertFalse(report["complete"])

    def test_missing_incoming_ref_is_unavailable_not_clean(self):
        report = self.run_audit(incoming_ref="does-not-exist")
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "revision_unavailable")

    def test_output_limit_never_yields_a_partial_clean_result(self):
        for i in range(70):
            self.write(f".beads/{i}.db")
        self.commit(".beads")
        report = self.run_audit(max_bytes=1024)
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "probe_output_limit")

    def test_expired_budget_is_unavailable(self):
        report = self.run_audit(timeout=0)
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "probe_timed_out")

    def test_missing_git_is_unavailable(self):
        with mock.patch.object(audit.shutil, "which", return_value=None):
            report = self.run_audit()
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "git_unavailable")

    def test_corrupt_index_cannot_be_reported_clean(self):
        (self.repo / ".git/index").write_bytes(b"not a Git index")
        report = self.run_audit()
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "git_probe_failed")

    def test_capture_directory_inside_worktree_is_refused_before_creation(self):
        captures = self.repo / "captures"
        with mock.patch.object(audit.tempfile, "gettempdir", return_value=str(captures)):
            report = self.run_audit()
        self.assertEqual(report["reason"], "capture_directory_inside_worktree")
        self.assertFalse(captures.exists())

    @unittest.skipUnless(os.name == "posix", "executable fixture requires POSIX")
    def test_live_probe_timeout_kills_and_reaps_the_direct_process(self):
        pid_path = self.base / "probe.pid"
        fake = self.base / "fake-git"
        fake.write_text(f"#!{sys.executable} -S\nimport os,time\nopen({str(pid_path)!r},'w').write(str(os.getpid()))\ntime.sleep(60)\n")
        fake.chmod(0o700)
        started = time.monotonic()
        spawned = []
        real_popen = subprocess.Popen
        def record_child(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            spawned.append(child)
            return child
        with mock.patch.object(audit.shutil, "which", return_value=str(fake)):
            with mock.patch.object(audit.subprocess, "Popen", side_effect=record_child):
                report = self.run_audit(timeout=0.2)
        self.assertEqual(report["reason"], "probe_timed_out")
        self.assertLess(time.monotonic() - started, 3)
        self.assertEqual(len(spawned), 1)
        self.assertIsNotNone(spawned[0].returncode)
        with self.assertRaises(ProcessLookupError):
            os.kill(spawned[0].pid, 0)

    @unittest.skipUnless(os.name == "posix", "executable fixture requires POSIX")
    def test_stderr_output_limit_kills_and_reaps_the_direct_process(self):
        pid_path = self.base / "probe.pid"
        fake = self.base / "fake-git"
        fake.write_text(f"#!{sys.executable} -S\nimport os,time\nopen({str(pid_path)!r},'w').write(str(os.getpid()))\nos.write(2,b'x'*2048)\ntime.sleep(60)\n")
        fake.chmod(0o700)
        with mock.patch.object(audit.shutil, "which", return_value=str(fake)):
            report = self.run_audit(max_bytes=1024)
        self.assertEqual(report["reason"], "probe_output_limit")
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_path.read_text()), 0)

    def test_final_stderr_write_is_checked_after_process_exit(self):
        class FinalWriteProcess:
            def __init__(self, *args, stdout, stderr, **kwargs):
                self.stdout, self.stderr = stdout, stderr
                self.returncode = None

            def poll(self):
                if self.returncode is None:
                    self.stdout.write(b"true\n")
                    self.stdout.flush()
                    self.stderr.write(b"x" * 2048)
                    self.stderr.flush()
                    self.returncode = 0
                return self.returncode

            def wait(self):
                return self.returncode

        git = audit.Git(self.repo, timeout=5, max_bytes=1024)
        with mock.patch.object(audit.subprocess, "Popen", FinalWriteProcess):
            with self.assertRaisesRegex(audit.AuditFailure, "probe_output_limit"):
                git.run("rev-parse", "--is-inside-work-tree")

    def test_moving_incoming_ref_invalidates_observation(self):
        self.write(".beads/beads.db", b"first")
        old = self.commit(".beads")
        self.write(".beads/beads.db", b"second")
        new = self.commit(".beads")
        self.git("update-ref", "refs/heads/incoming", old)
        original = audit.Git.commit
        changed = False
        def intercepted(git, revision, **kwargs):
            nonlocal changed
            result = original(git, revision, **kwargs)
            if revision == "incoming" and not changed:
                changed = True
                self.git("update-ref", "refs/heads/incoming", new)
            return result
        with mock.patch.object(audit.Git, "commit", intercepted):
            report = self.run_audit(incoming_ref="incoming")
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "incoming_ref_changed_during_audit")

    def test_changed_index_during_audit_is_unavailable(self):
        original = audit.Git.required
        count = 0
        def intercepted(git, *args):
            nonlocal count
            result = original(git, *args)
            if args[0] == "ls-files":
                count += 1
                if count == 1:
                    self.write(".beads/new.db")
                    self.git("add", ".beads/new.db")
            return result
        with mock.patch.object(audit.Git, "required", intercepted):
            report = self.run_audit()
        self.assertFalse(report["complete"])
        self.assertEqual(report["reason"], "repository_changed_during_audit")

    def test_inherited_git_redirection_cannot_target_another_repository(self):
        self.write(".beads/beads.db")
        self.commit(".beads")
        with mock.patch.dict(os.environ, {"GIT_DIR": str(self.base / "missing.git"),
                                          "GIT_INDEX_FILE": str(self.base / "fake-index"),
                                          "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.fsmonitor",
                                          "GIT_CONFIG_VALUE_0": "malicious-hook"}):
            report = self.run_audit()
        self.assertEqual(report["status"], "unsafe", report)
        self.assertFalse((self.base / "fake-index").exists())

    def test_audit_preserves_entire_worktree_and_index_bytes_and_inodes(self):
        self.write(".beads/beads.db", b"do not open this database")
        self.write(".beads/beads.db-wal-cert", b"malformed but retained certificate")
        self.write(".beads/issues.jsonl", b"[]\n")
        self.commit(".beads")
        def witness():
            return {str(p.relative_to(self.repo)): (p.stat().st_dev, p.stat().st_ino, p.read_bytes())
                    for p in self.repo.rglob("*") if p.is_file()}
        before = witness()
        self.assertEqual(self.run_audit()["status"], "unsafe")
        self.assertEqual(witness(), before)

    @unittest.skipUnless(os.name == "posix", "hook script requires POSIX")
    def test_fsmonitor_and_content_filters_never_execute(self):
        sentinel = self.base / "hook-ran"
        hook = self.base / "hook.sh"
        hook.write_text(f"#!/bin/sh\nprintf unsafe > '{sentinel}'\n")
        hook.chmod(0o700)
        self.write(".beads/beads.db")
        self.commit(".beads")
        self.git("config", "core.fsmonitor", str(hook))
        self.git("config", "filter.bad.clean", str(hook))
        self.git("config", "filter.bad.smudge", str(hook))
        self.write(".gitattributes", b"* filter=bad\n")
        report = self.run_audit()
        self.assertEqual(report["status"], "unsafe", report)
        self.assertFalse(sentinel.exists())

    def test_fleet_keeps_successful_evidence_when_one_repository_fails(self):
        self.write(".beads/beads.db")
        self.commit(".beads")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = audit.main([str(self.repo), str(self.base / "missing"), "--json"])
        report = json.loads(output.getvalue())
        self.assertEqual(code, 2)
        self.assertEqual([r["status"] for r in report["repositories"]], ["unsafe", "unavailable"])
        self.assertEqual(report["schema"], audit.SCHEMA)

    def test_parser_rejects_truncated_duplicate_and_inconsistent_records(self):
        oid = b"0" * 40
        valid = b"100644 " + oid + b" 0\t.beads/beads.db\0"
        for bad in (valid[:-1], valid + valid, b"\0", b"100644 bad 0\tpath\0",
                    b"100644 " + oid + b" 0\t../escape\0",
                    b"100644 " + oid + b" 4\tpath\0",
                    valid + b"100644 " + oid + b" 1\t.beads/beads.db\0"):
            with self.subTest(bad=bad):
                with self.assertRaises(audit.AuditFailure):
                    audit.parse_entries(bad, tree=False)
        with self.assertRaises(audit.AuditFailure):
            audit.parse_entries(b"100644 commit " + oid + b"\tfile\0", tree=True)

    def test_scope_validation_refuses_traversal_metadata_and_pathspec_injection(self):
        for value in ("", ".", "..", "a/../b", "/tmp", "a//b", "./.beads", ".git", "a/.GIT/index", "a\\b", ":(glob)*", "bad\0name"):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    audit.relative_path(value)


if __name__ == "__main__":
    unittest.main(verbosity=2)
