#!/usr/bin/env python3
"""Exercise the actual qualification workflow's Python gates without Cargo.

Only temporary manifests/metadata are modified. These tests prove neither Rust
compilation nor database compatibility; they prevent a mixed-family resolution
or an unsafe candidate argument from masquerading as a valid qualification lane.
The repository's real manifest and lockfile are checked as well as fixtures.
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
import tomllib
import unittest

REPOSITORY = Path(__file__).resolve().parents[1]
DEFAULT_WORKFLOW = REPOSITORY / ".github/workflows/fsqlite-admission-qualification.yml"
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


def manifest(version: str = "0.4.10", *, vfs: str | None = None) -> str:
    lines = ["[package]", "name = 'disposable-gate-fixture'", "version = '0.0.0'",
             "[dependencies]", "serde = '1'"]
    for name in NAMES:
        pin = vfs if name == "fsqlite-vfs" and vfs else version
        lines.append(f'{name} = {{ version = "={pin}" }}')
    return "\n".join(lines) + "\n"


def metadata(version: str = "0.4.12", *, vfs: str | None = None) -> dict:
    # Include an indirect family member: checking just the root manifest is
    # insufficient to certify a coherent engine type/namespace implementation.
    packages = []
    for name in (*NAMES, "fsqlite-pager"):
        resolved = vfs if name == "fsqlite-vfs" and vfs else version
        packages.append({"name": name, "version": resolved, "source": SOURCE,
                         "id": f"{SOURCE}#{name}@{resolved}"})
    return {"packages": packages}


def expected(version: str = "0.4.12", *, vfs: str | None = None) -> dict:
    pins = dict.fromkeys(NAMES, version)
    if vfs:
        pins["fsqlite-vfs"] = vfs
    return {"version": version, "direct": sorted(NAMES), "pins": pins}


def lock_metadata(source: str) -> dict:
    """Adapt the real locked package identities to the workflow gate's input.

    Cargo itself still verifies dependency edges and checksums in the locked
    build. This adapter deliberately preserves duplicate versions and sources.
    """
    packages = tomllib.loads(source)["package"]
    return {"packages": [
        {"name": p["name"], "version": p["version"], "source": p.get("source"),
         "id": f'{p.get("source")}#{p["name"]}@{p["version"]}'}
        for p in packages
    ]}


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
                         expected("0.4.10"))

    def test_locked_lane_preserves_audited_vfs_backport(self):
        original = manifest(vfs="0.4.11")
        result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original}, lane="locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["Cargo.toml"], original)
        self.assertEqual(json.loads(outputs["expected-family.json"]),
                         expected("0.4.10", vfs="0.4.11"))
        result, verified = self.run_gate(self.verify, {
            "expected-family.json": outputs["expected-family.json"],
            "engine-metadata.json": json.dumps(metadata("0.4.10", vfs="0.4.11")),
        }, lane="locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({p["name"]: p["version"]
                          for p in json.loads(verified["resolved-family.json"])},
                         {**expected("0.4.10", vfs="0.4.11")["pins"],
                          "fsqlite-pager": "0.4.10"})

    def test_candidate_updates_all_direct_pins_and_nothing_else(self):
        for original in (manifest(), manifest(vfs="0.4.11")):
            with self.subTest(original=original):
                result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(outputs["Cargo.toml"], manifest("0.4.12"))
                self.assertEqual(json.loads(outputs["expected-family.json"]), expected())

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
            (manifest(vfs="0.5.0"), "mixes engine-family pins"),
            (manifest(vfs="0.4.12"), "mixes engine-family pins"),
            (manifest("0.4.12", vfs="0.4.11"), "mixes engine-family pins"),
            (original.replace('"=0.4.10"', '"0.4.10"'), "unrecognized or unpinned"),
            (original.replace('"=0.4.10"', '"=0.4.10", path = "../engine"'), "unrecognized or unpinned"),
            (original.replace('"=0.4.10"', '"=0.4.10", git = "https://example.invalid/engine"'),
             "unrecognized or unpinned"),
            (original.replace('"=0.4.10"', '"=0.4.10", registry = "private"'),
             "unrecognized or unpinned"),
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
            "expected-family.json": json.dumps(expected()),
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
        cases.append((metadata(vfs="0.4.11"), "mixed engine-family versions"))
        for source in (None, "git+https://example.invalid/engine#123",
                       "registry+https://example.invalid/index"):
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
                    "expected-family.json": json.dumps(expected()),
                    "engine-metadata.json": json.dumps(resolved),
                })
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stderr)
                self.assertNotIn("resolved-family.json", outputs)

    def test_locked_resolution_requires_exact_backport_and_complete_family(self):
        good = metadata("0.4.10", vfs="0.4.11")
        cases = []
        for vfs in ("0.4.10", "0.4.12", "0.5.0"):
            cases.append(metadata("0.4.10", vfs=vfs))
        missing = copy.deepcopy(good)
        missing["packages"] = [p for p in missing["packages"] if p["name"] != "fsqlite-error"]
        cases.append(missing)
        unexpected = copy.deepcopy(good)
        unexpected["packages"][-1]["version"] = "0.5.0"
        cases.append(unexpected)
        duplicate = copy.deepcopy(good)
        duplicate["packages"].extend(metadata("0.5.0")["packages"])
        cases.append(duplicate)
        for resolved in cases:
            with self.subTest(resolved=resolved):
                result, outputs = self.run_gate(self.verify, {
                    "expected-family.json": json.dumps(expected("0.4.10", vfs="0.4.11")),
                    "engine-metadata.json": json.dumps(resolved),
                }, lane="locked")
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("resolved-family.json", outputs)

    def test_repository_manifest_and_lock_have_one_audited_engine_family(self):
        original = (REPOSITORY / "Cargo.toml").read_text()
        result, outputs = self.run_gate(self.prepare, {"Cargo.toml": original}, lane="locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["Cargo.toml"], original)
        resolved = lock_metadata((REPOSITORY / "Cargo.lock").read_text())
        result, verified = self.run_gate(self.verify, {
            "expected-family.json": outputs["expected-family.json"],
            "engine-metadata.json": json.dumps(resolved),
        }, lane="locked")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("resolved-family.json", verified)

        # The synchronous bridge and optional MCP server exchange Asupersync
        # types. A second runtime identity would reintroduce the same problem.
        runtime_pin = tomllib.loads(original)["dependencies"]["asupersync"]["version"]
        self.assertTrue(runtime_pin.startswith("="), "Asupersync must be pinned exactly")
        runtimes = [(p["version"], p["source"]) for p in resolved["packages"]
                    if p["name"] == "asupersync"]
        self.assertEqual(runtimes, [(runtime_pin[1:], SOURCE)],
                         "bridge, engine and MCP must resolve one published Asupersync identity")


if __name__ == "__main__":
    unittest.main(verbosity=2)
