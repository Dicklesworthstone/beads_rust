#!/usr/bin/env python3
"""Exercise the actual qualification workflow's Python gates without Cargo.

Only temporary manifests/metadata are modified. These tests prove neither Rust
compilation nor database compatibility; they prevent a mixed-family resolution
or an unsafe candidate argument from masquerading as a valid qualification lane.
"""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

DEFAULT_WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/fsqlite-admission-qualification.yml"
WORKFLOW = Path(sys.argv.pop(1)) if len(sys.argv) > 1 and sys.argv[1].endswith(".yml") else DEFAULT_WORKFLOW
NAMES = ("fsqlite", "fsqlite-error", "fsqlite-vfs", "fsqlite-wal")
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


def scripts(path: Path) -> list[str]:
    blocks = []
    for rest in path.read_text().split("python3 -I - <<'PY'\n")[1:]:
        lines = []
        for line in rest.splitlines():
            if line.strip() == "PY":
                break
            lines.append(line)
        else:
            raise AssertionError("unterminated workflow Python block")
        blocks.append(textwrap.dedent("\n".join(lines)))
    if len(blocks) != 2:
        raise AssertionError("qualification workflow must expose both preparation and resolution gates")
    return blocks


def manifest(version: str = "0.4.10") -> str:
    return ("[package]\nname = 'disposable-gate-fixture'\nversion = '0.0.0'\n"
            "[dependencies]\nserde = '1'\n" + "\n".join(
                f'{name} = {{ version = "={version}" }}' for name in NAMES) + "\n")


def metadata(version: str = "0.4.12") -> dict:
    # Include an indirect family member: checking just the root manifest is
    # insufficient to certify a coherent engine type/namespace implementation.
    return {"packages": [{"name": name, "version": version, "source": SOURCE,
                           "id": f"{SOURCE}#{name}@{version}"}
                          for name in (*NAMES, "fsqlite-pager")]}


class FamilyGateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.prepare, cls.verify = scripts(WORKFLOW)

    def run_gate(self, code, files, *, lane="candidate", candidate="0.4.12"):
        with tempfile.TemporaryDirectory(prefix="br-family-gate-") as temporary:
            root = Path(temporary)
            for name, content in files.items():
                (root / name).write_text(content)
            result = subprocess.run([sys.executable, "-I", "-c", code], cwd=root,
                env={**os.environ, "PYTHONOPTIMIZE": "2", "ENGINE_LANE": lane, "CANDIDATE_VERSION": candidate},
                capture_output=True, text=True, timeout=10, check=False)
            outputs = {p.name: p.read_text() for p in root.iterdir() if p.is_file()}
            return result, outputs

    def test_locked_lane_preserves_every_manifest_byte(self):
        original = manifest()
        result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original}, lane="locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["Cargo.toml"], original)
        self.assertEqual(json.loads(outputs["expected-family.json"]),
                         {"version": "0.4.10", "direct": sorted(NAMES)})

    def test_candidate_updates_all_direct_pins_and_nothing_else(self):
        original = manifest()
        result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["Cargo.toml"], original.replace("=0.4.10", "=0.4.12"))
        self.assertEqual(json.loads(outputs["expected-family.json"])["version"], "0.4.12")

    def test_invalid_candidate_arguments_fail_before_manifest_mutation(self):
        for value in ("", "0.4", "0.4.12; echo unsafe", "0.4.12\nanything", "../../main", "v0.4.12"):
            with self.subTest(candidate=value):
                original = manifest()
                result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original}, candidate=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("candidate must be a release version", result.stderr)
                self.assertEqual(outputs["Cargo.toml"], original)
                self.assertNotIn("expected-family.json", outputs)

    def test_mixed_unpinned_and_path_manifests_fail_closed(self):
        original = manifest()
        changed = [
            (original.replace('fsqlite-wal = { version = "=0.4.10" }',
                              'fsqlite-wal = { version = "=0.4.12" }'), "mixes engine-family pins"),
            (original.replace('"=0.4.10"', '"0.4.10"'), "unrecognized or unpinned"),
            (original.replace('"=0.4.10"', '"=0.4.10", path = "../engine"'), "unrecognized or unpinned"),
        ]
        for source, diagnostic in changed:
            with self.subTest(diagnostic=diagnostic):
                result, outputs = self.run_gate(self.prepare, {"Cargo.toml": source})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stderr)
                self.assertEqual(outputs["Cargo.toml"], source)

    def test_complete_published_resolution_is_recorded(self):
        resolved = metadata()
        result, outputs = self.run_gate(self.verify, {
            "expected-family.json": json.dumps({"version": "0.4.12", "direct": list(NAMES)}),
            "engine-metadata.json": json.dumps(resolved),
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({p["name"] for p in json.loads(outputs["resolved-family.json"])},
                         set(NAMES) | {"fsqlite-pager"})

    def test_transitive_version_source_and_duplicate_identity_failures(self):
        good = metadata()
        cases = []
        mixed = copy.deepcopy(good)
        mixed["packages"][-1]["version"] = "0.4.10"
        cases.append((mixed, "mixed engine-family versions"))
        for source in (None, "git+https://example.invalid/engine#123"):
            changed = copy.deepcopy(good)
            for package in changed["packages"]:
                package["source"] = source
            cases.append((changed, "mixed/unpublished engine sources"))
        duplicate = copy.deepcopy(good)
        duplicate["packages"].append(copy.deepcopy(duplicate["packages"][0]))
        cases.append((duplicate, "duplicate engine crate identities"))
        for resolved, diagnostic in cases:
            with self.subTest(diagnostic=diagnostic):
                result, outputs = self.run_gate(self.verify, {
                    "expected-family.json": json.dumps({"version": "0.4.12", "direct": list(NAMES)}),
                    "engine-metadata.json": json.dumps(resolved),
                })
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stderr)
                self.assertNotIn("resolved-family.json", outputs)


if __name__ == "__main__":
    unittest.main(verbosity=2)
