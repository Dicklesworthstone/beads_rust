#!/usr/bin/env python3
"""Issues #554/#577: clean CLI imports do not create compaction backup sets.

Real imports that need compaction are covered by tests/e2e_sync_artifacts.rs.
Age/count and adversarial-filesystem tests live in config::compaction_retention.
No receipt is fabricated or edited to pretend that a compaction completed.
"""
import datetime
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile


def main() -> None:
    binary = pathlib.Path(sys.argv[1]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="br-retention-imports-") as directory:
        # macOS's system temp path traverses /var -> /private/var. Keep the
        # fixture canonical without relaxing the production route guard.
        root = pathlib.Path(directory).resolve(strict=True)
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("BD_", "BEADS_", "BR_", "TOON_"))
        }
        env.update({
            "BEADS_DIR": str(root / ".beads"),
            "HOME": str(root),
            "NO_COLOR": "1",
            "RUST_LOG": "error",
        })

        def run(*args: str) -> str:
            result = subprocess.run(
                [str(binary), *args], cwd=root, check=False,
                capture_output=True, text=True, timeout=120, env=env,
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

        def recovery_snapshot() -> dict[str, str]:
            return {
                str(path.relative_to(recovery)): (
                    hashlib.sha256(path.read_bytes()).hexdigest()
                    if path.is_file() else "directory"
                )
                for path in recovery.rglob("*")
            }

        prior = recovery_snapshot()
        started = datetime.datetime.now(datetime.timezone.utc)
        for generation in range(8):
            records = [json.loads(line) for line in source.read_text().splitlines() if line.strip()]
            assert len(records) == 1, records
            records[0]["title"] = f"retention generation {generation}"
            records[0]["updated_at"] = (
                started + datetime.timedelta(seconds=generation + 1)
            ).isoformat()
            source.write_text("".join(json.dumps(record) + "\n" for record in records))
            imported = json.loads(run("sync", "--import-only", "--json"))
            assert imported["updated"] == 1, imported
            assert recovery_snapshot() == prior, (
                f"clean import changed compaction recovery evidence: generation {generation}"
            )
            shown = json.loads(run("show", records[0]["id"], "--json"))
            assert shown[0]["title"] == records[0]["title"], shown
            assert recovery_snapshot() == prior, "reading the issue changed recovery evidence"
        print("PASS: eight clean CLI imports preserve data without creating compaction backups")


if __name__ == "__main__":
    main()
