#!/usr/bin/env python3
"""Issue #554: real CLI imports seal fresh compactions and protect recent sets.

Age/count and adversarial-filesystem tests live in config::compaction_retention.
This test deliberately does not edit receipts to pretend a compaction completed.
"""
import datetime
import json
import pathlib
import subprocess
import sys
import tempfile


def main() -> None:
    binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="br-retention-imports-") as directory:
        root = pathlib.Path(directory)

        def run(*args: str) -> str:
            result = subprocess.run(
                [str(binary), *args], cwd=root, check=False,
                capture_output=True, text=True, timeout=120,
            )
            if result.returncode:
                raise AssertionError(
                    f"br {args!r}: exit {result.returncode}\n"
                    f"stdout: {result.stdout}\nstderr: {result.stderr}"
                )
            return result.stdout

        run("init", "--prefix", "r554")
        run("create", "retention fixture", "--type", "task")
        run("sync", "--flush-only")
        source = root / ".beads" / "issues.jsonl"
        recovery = root / ".beads" / ".br_recovery"
        prior = {p.name: p.read_bytes() for p in recovery.glob("*.complete.json")}
        for generation in range(8):
            records = [json.loads(line) for line in source.read_text().splitlines() if line.strip()]
            assert len(records) == 1, records
            records[0]["title"] = f"retention generation {generation}"
            records[0]["updated_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            source.write_text("".join(json.dumps(record) + "\n" for record in records))
            run("sync", "--import-only")
            current = {p.name: p.read_bytes() for p in recovery.glob("*.complete.json")}
            assert len(current) > len(prior), "successful import did not seal a new compaction"
            for name, contents in prior.items():
                assert current.get(name) == contents, f"recent completion proof changed: {name}"
            for name, contents in current.items():
                receipt = json.loads(contents)
                assert receipt["schema_version"] == "br.compaction.complete.v1", name
                database = pathlib.Path(receipt["database_path"]).name
                for member in receipt["members"]:
                    backup = recovery / f'{database}{member["suffix"]}.{receipt["stamp"]}.bak'
                    assert backup.is_file(), f"recent backup was removed: {backup}"
            shown = run("show", records[0]["id"], "--json")
            assert records[0]["title"] in shown, shown
            prior = current
        print("PASS: eight real imports seal complete families without pruning recent backups")


if __name__ == "__main__":
    main()
