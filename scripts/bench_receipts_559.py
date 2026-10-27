#!/usr/bin/env python3
"""Run the unchanged release_abba collector for issue #559's exact 10k corpus.

The test driver owns dataset generation, ABBA order, quiet-runner admission,
mutation correctness, and statistics. This wrapper supplies immutable artifacts,
an external cohort timeout, A/A calibration, and a descriptive report. Forty
blocks yield 80 retained observations per side, below the release gate minimum.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import unittest


WORKLOADS = tuple(
    f"10000-{command}-{mode}"
    for command in ("create", "update")
    for mode in ("default-auto-flush", "diagnostic-no-auto-flush")
)
TEST = "release_abba::release_cli_abba_1k_10k"


def read_json(path):
    return json.loads(path.read_text())


def pins(artifacts, side):
    before = side == "baseline"
    binary = artifacts / ("br-before" if before else "br-after")
    source = (artifacts / f"{side}.source").read_text().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        raise ValueError(f"invalid immutable {side} source revision")
    return {
        "BINARY": str(binary),
        "SHA256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "SOURCE": source,
        "LOCKFILE": str(artifacts / f"{side}.Cargo.lock"),
    }


def collector_environment(inherited, baseline, candidate, workload, output):
    # Inherited negative controls or budgets must not contaminate this campaign.
    env = {k: v for k, v in inherited.items() if not k.startswith("BR_PERF_")}
    env.update({
        "BR_PERF_QUIET_RUNNER": "1",
        "BR_PERF_ABBA_BLOCKS": "40",
        "BR_PERF_WORKLOAD": workload,
        "BR_PERF_OUTPUT": str(output),
    })
    for side, identity in (("BASELINE", baseline), ("CANDIDATE", candidate)):
        env.update({f"BR_PERF_{side}_{k}": v for k, v in identity.items()})
    return env


def calibration(interval):
    if not interval or interval["lower"] is None or interval["upper"] is None:
        return "unbounded; inconclusive"
    if interval["lower"]["delta_pct"] <= 0 <= interval["upper"]["delta_pct"]:
        return "median interval includes zero"
    return "median interval excludes zero; do not infer an A/B effect"


def collect(artifacts, output):
    output.mkdir(parents=True, exist_ok=False)
    baseline, candidate = pins(artifacts, "baseline"), pins(artifacts, "candidate")
    results, boot_ids = [], set()
    for workload in WORKLOADS:
        aa_ok = False
        for cohort in ("aa", "ab"):
            directory = output / cohort / workload
            directory.mkdir(parents=True)
            row = {"cohort": cohort, "workload": workload}
            results.append(row)
            if cohort == "ab" and not aa_ok:
                row["error"] = "A/A collection failed; A/B skipped"
                continue
            env = collector_environment(
                os.environ, candidate if cohort == "aa" else baseline,
                candidate, workload, directory,
            )
            try:
                with (directory / "collector.log").open("w") as log:
                    subprocess.run(
                        [str(artifacts / "measurement-driver"), "--exact", TEST,
                         "--ignored", "--nocapture"],
                        env=env, stdout=log, stderr=subprocess.STDOUT,
                        timeout=1800, check=True,
                    )
                run = read_json(directory / "run.json")
                if (run["state"], run["selected_workload"], run["completed_workloads"]) != (
                    "measurements_completed", workload, 1
                ):
                    raise ValueError("collector did not complete the exact workload")
                summary = read_json(directory / f"{workload}-summary.json")
                for side in ("baseline", "candidate"):
                    if summary[side]["sample_count"] != 80:
                        raise ValueError("expected 80 retained samples on each side")
                for side in (0, 1):
                    measured = read_json(directory / f"{workload}-{side}.json")
                    boot_ids.add(measured["metadata"]["host_boot_id"])
                if len(boot_ids) != 1:
                    raise ValueError("A/A and A/B must share one kernel boot identity")
                row["summary"] = summary
                if cohort == "aa":
                    uncertainty = summary["comparison"].get("uncertainty")
                    row["calibration"] = calibration(uncertainty.get("median") if uncertainty else None)
                    aa_ok = True
            except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
                row["error"] = str(error)
            finally:
                (output / "report.json").write_text(json.dumps({
                    "baseline": baseline, "candidate": candidate,
                    "release_gate": "inconclusive: descriptive 40-block campaign; no budget supplied",
                    "results": results,
                }, indent=2) + "\n")
    lines = [
        "# Issue #559: 10,000-item receipt benchmark", "",
        f"Baseline: `{baseline['SOURCE']}`. Candidate: `{candidate['SOURCE']}`.", "",
        "Matched repository release profile (`opt-level=z`), unchanged synthetic collector, seed 42, "
        "dependency density 0.25, 16 labels, no comments or claims. Each workload uses two warmup "
        "blocks and 40 retained ABBA blocks (80 samples per side), fresh workspace copies, and "
        "direct CLI children. Fixture copying and correctness checks are outside measured intervals.", "",
        "This is descriptive evidence, not a release-gate pass. The collector retains its quiet-runner "
        "admission, A/A calibration, raw observations, and conditional IID-block uncertainty. "
        "No timing budget was supplied; p95 remains descriptive. An A/A interval excluding zero "
        "prevents interpreting the corresponding A/B change as an established effect.", "",
        "| Cohort | Workload | Baseline median (ms) | Candidate median (ms) | Median delta | Baseline p95 (ms) | Candidate p95 (ms) | A/A calibration |",
        "|---|---|---:|---:|---:|---:|---:|---|",
    ]
    for row in results:
        if "summary" not in row:
            lines.append(f"| {row['cohort']} | {row['workload']} | FAILED | | | | | {row.get('error', 'missing result')} |")
            continue
        summary = row["summary"]
        a, b = summary["baseline"], summary["candidate"]
        delta = (b["median_ms"] / a["median_ms"] - 1) * 100
        lines.append(
            f"| {row['cohort']} | {row['workload']} | {a['median_ms']:.3f} | "
            f"{b['median_ms']:.3f} | {delta:+.2f}% | {a['p95_ms']:.3f} | "
            f"{b['p95_ms']:.3f} | {row.get('calibration', 'see preceding A/A')} |"
        )
    # Include skipped final cohorts as well as the last completed collector.
    (output / "report.json").write_text(json.dumps({
        "baseline": baseline, "candidate": candidate,
        "release_gate": "inconclusive: descriptive 40-block campaign; no budget supplied",
        "results": results,
    }, indent=2) + "\n")
    (output / "report.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    return int(any("error" in row for row in results))


class Controls(unittest.TestCase):
    def test_inherited_controls_cannot_change_campaign(self):
        identity = {"BINARY": "/candidate", "SHA256": "digest", "SOURCE": "revision", "LOCKFILE": "/lock"}
        env = collector_environment(
            {"PATH": "/bin", "BR_PERF_ABBA_BLOCKS": "999", "BR_PERF_BUDGET_PCT": "1000",
             "BR_PERF_NEGATIVE_EXTRA_VERSION_CALLS": "3"},
            identity, identity, WORKLOADS[0], Path("/evidence"),
        )
        self.assertEqual(env["BR_PERF_ABBA_BLOCKS"], "40")
        self.assertEqual(env["BR_PERF_QUIET_RUNNER"], "1")
        self.assertNotIn("BR_PERF_BUDGET_PCT", env)
        self.assertNotIn("BR_PERF_NEGATIVE_EXTRA_VERSION_CALLS", env)
        self.assertEqual(env["BR_PERF_BASELINE_BINARY"], env["BR_PERF_CANDIDATE_BINARY"])
        self.assertEqual(env["PATH"], "/bin")

    def test_calibration_does_not_call_biased_or_missing_bounds_clean(self):
        self.assertIn("inconclusive", calibration(None))
        self.assertIn("includes zero", calibration({"lower": {"delta_pct": -1}, "upper": {"delta_pct": 1}}))
        self.assertIn("do not infer", calibration({"lower": {"delta_pct": 1}, "upper": {"delta_pct": 2}}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Controls))
        return int(not result.wasSuccessful())
    if args.artifacts is None or args.output is None:
        parser.error("--artifacts and --output are required")
    return collect(args.artifacts.resolve(), args.output.resolve())


if __name__ == "__main__":
    sys.exit(main())
