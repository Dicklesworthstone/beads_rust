#!/usr/bin/env bash
# Report workflow action pin update status without mutating repository files.

set -euo pipefail

python3 - "$@" <<'PY'
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path


SHA40 = re.compile(r"^[0-9a-fA-F]{40}$")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Report whether checked-in GitHub Action pins match the configured allowed upstream refs."
    )
    parser.add_argument("--inventory", default=".github/action-pins.jsonl")
    parser.add_argument("--upstreams", default=".github/action-pin-upstreams.jsonl")
    parser.add_argument("--format", choices=("json", "text"), default="json")
    parser.add_argument("--all", action="store_true", help="Include up-to-date rows in text output.")
    parser.add_argument(
        "--live",
        action="store_true",
        help="Resolve configured upstream refs with git ls-remote instead of trusting latest_allowed_sha.",
    )
    parser.add_argument("--timeout", type=float, default=10.0)
    return parser.parse_args()


def load_jsonl(path: Path, *, required: bool) -> list[dict[str, object]]:
    if not path.exists():
        if required:
            raise SystemExit(f"required JSONL file not found: {path}")
        return []

    rows: list[dict[str, object]] = []
    for index, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise SystemExit(f"{path}:{index} invalid JSON: {error}") from error
        if not isinstance(value, dict):
            raise SystemExit(f"{path}:{index} must contain a JSON object")
        rows.append(value)
    return rows


def text_field(row: dict[str, object], field: str) -> str:
    value = row.get(field, "")
    return value if isinstance(value, str) else ""


def validate_inventory(rows: list[dict[str, object]], path: Path) -> None:
    if not rows:
        raise SystemExit(f"{path} has no action pin entries")
    for index, row in enumerate(rows, start=1):
        for field in ("workflow", "action", "sha", "tag"):
            if not text_field(row, field):
                raise SystemExit(f"{path}:{index} missing required field {field!r}")
        if not SHA40.fullmatch(text_field(row, "sha")):
            raise SystemExit(f"{path}:{index} sha must be a 40-character hex value")


def upstream_index(rows: list[dict[str, object]], path: Path) -> dict[str, dict[str, object]]:
    by_action: dict[str, dict[str, object]] = {}
    for index, row in enumerate(rows, start=1):
        action = text_field(row, "action")
        if not action:
            raise SystemExit(f"{path}:{index} missing required field 'action'")
        if action in by_action:
            raise SystemExit(f"{path}:{index} duplicate upstream policy for {action}")
        by_action[action] = row
    return by_action


def version_parts(tag: str) -> list[int] | None:
    match = re.fullmatch(r"v?(\d+(?:\.\d+)*)", tag)
    if match is None:
        return None
    parts = [int(part) for part in match.group(1).split(".")]
    while parts and parts[-1] == 0:
        parts.pop()
    return parts


def compare_versions(left: str, right: str) -> int | None:
    left_parts = version_parts(left)
    right_parts = version_parts(right)
    if left_parts is None or right_parts is None:
        return None
    width = max(len(left_parts), len(right_parts))
    left_parts.extend([0] * (width - len(left_parts)))
    right_parts.extend([0] * (width - len(right_parts)))
    return (left_parts > right_parts) - (left_parts < right_parts)


def ls_remote_ref(repo: str, tag: str, timeout: float) -> tuple[str, str]:
    refs = ["HEAD"] if tag.startswith("default-branch-head-") else [f"refs/tags/{tag}^{{}}", f"refs/tags/{tag}"]
    try:
        result = subprocess.run(
            ["git", "ls-remote", repo, *refs],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "upstream_unreachable", ""

    if result.returncode != 0:
        return "upstream_unreachable", ""

    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[1].endswith("^{}") and SHA40.fullmatch(fields[0]):
            return "ok", fields[0]
    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) >= 2 and SHA40.fullmatch(fields[0]):
            return "ok", fields[0]
    return "missing_tag", ""


def upstream_status(upstream: dict[str, object] | None, *, live: bool, timeout: float) -> tuple[str, str, str, str]:
    if upstream is None:
        return "unconfigured", "", "", ""

    configured_status = text_field(upstream, "lookup_status")
    tag = text_field(upstream, "latest_allowed_tag")
    repo = text_field(upstream, "repo")
    if configured_status in {"upstream_unreachable", "missing_tag"}:
        return configured_status, tag, text_field(upstream, "latest_allowed_sha"), repo
    if not tag:
        return "config_error", "", "", repo

    if live:
        if not repo:
            return "config_error", tag, "", repo
        status, sha = ls_remote_ref(repo, tag, timeout)
        return status, tag, sha, repo

    sha = text_field(upstream, "latest_allowed_sha")
    if not SHA40.fullmatch(sha):
        return "config_error", tag, sha, repo
    return "ok", tag, sha, repo


def manual_steps(entry: dict[str, str]) -> list[str]:
    action = entry["action"]
    latest_tag = entry["latest_allowed_tag"]
    latest_sha = entry["latest_allowed_sha"]
    if entry["status"] in {"upstream_unreachable", "missing_tag", "config_error", "unconfigured"}:
        return [
            f"Resolve and record an allowed upstream ref for {action}.",
            "Do not change workflow pins until the upstream SHA and provenance are known.",
        ]

    return [
        f"Review {action} upstream ref {latest_tag}.",
        f"Update each workflow uses entry for {action} to {latest_sha}.",
        f"Update .github/action-pins.jsonl rows for {action} with tag {latest_tag}, sha {latest_sha}, and provenance.",
        "Run ./scripts/verify-workflow-action-pins.sh and ubs on changed workflow/inventory files.",
    ]


def classify(
    current_tag: str,
    current_sha: str,
    upstream_lookup_status: str,
    latest_tag: str,
    latest_sha: str,
) -> str:
    if upstream_lookup_status != "ok":
        return upstream_lookup_status
    comparison = compare_versions(latest_tag, current_tag)
    if comparison is not None and comparison < 0:
        return "disallowed_downgrade"
    if latest_sha.lower() == current_sha.lower():
        return "up_to_date"
    return "update_available"


def build_report(args: argparse.Namespace) -> dict[str, object]:
    inventory_path = Path(args.inventory)
    upstreams_path = Path(args.upstreams)
    inventory = load_jsonl(inventory_path, required=True)
    upstreams = upstream_index(load_jsonl(upstreams_path, required=False), upstreams_path)
    validate_inventory(inventory, inventory_path)

    entries: list[dict[str, object]] = []
    resolved_upstreams: dict[str, tuple[str, str, str, str]] = {}
    for row in inventory:
        action = text_field(row, "action")
        current_tag = text_field(row, "tag")
        current_sha = text_field(row, "sha")
        if action not in resolved_upstreams:
            resolved_upstreams[action] = upstream_status(
                upstreams.get(action),
                live=args.live,
                timeout=args.timeout,
            )
        upstream_lookup, latest_tag, latest_sha, repo = resolved_upstreams[action]
        status = classify(current_tag, current_sha, upstream_lookup, latest_tag, latest_sha)
        entry = {
            "workflow": text_field(row, "workflow"),
            "action": action,
            "current_tag": current_tag,
            "current_sha": current_sha,
            "latest_allowed_tag": latest_tag,
            "latest_allowed_sha": latest_sha,
            "repo": repo,
            "upstream_status": upstream_lookup,
            "status": status,
        }
        entry["manual_update_steps"] = manual_steps(entry)
        entries.append(entry)

    entries.sort(key=lambda entry: (str(entry["action"]), str(entry["workflow"])))
    summary = Counter(str(entry["status"]) for entry in entries)
    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "inventory": str(inventory_path),
        "upstreams": str(upstreams_path),
        "live": args.live,
        "summary": {"total": len(entries), **dict(sorted(summary.items()))},
        "entries": entries,
    }


def emit_text(report: dict[str, object], *, show_all: bool) -> None:
    summary = report["summary"]
    entries = [
        entry
        for entry in report["entries"]
        if show_all or entry["status"] != "up_to_date"
    ]
    print("Action pin update audit")
    print(
        "Summary: "
        + ", ".join(f"{key}={value}" for key, value in summary.items() if key != "total")
        + f", total={summary['total']}"
    )
    if not entries:
        print("All action pins match configured upstream refs.")
        return
    for entry in entries:
        print(
            "- {status}: {action} in {workflow} "
            "current={current_tag}/{current_sha} latest={latest_allowed_tag}/{latest_allowed_sha}".format(
                **entry
            )
        )


def main() -> int:
    args = parse_args()
    report = build_report(args)
    if args.format == "json":
        json.dump(report, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
    else:
        emit_text(report, show_all=args.all)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
PY
