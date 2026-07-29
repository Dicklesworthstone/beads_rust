# CI Supply-Chain Maintenance

This project pins every external GitHub Action in `.github/workflows/*.yml` to a full 40-character commit SHA. The companion inventory at `.github/action-pins.jsonl` records the expected SHA and the human provenance note for each `(workflow, action)` pair.

The verifier is intentionally local and deterministic. It does not contact GitHub, create pull requests, push branches, or mutate files. It only compares workflow `uses:` entries against the checked-in inventory.

## Policy

All workflow changes must preserve these rules:

- External GitHub Actions must use immutable 40-character SHA refs in `uses:`.
- Every external action pin must have one matching `.github/action-pins.jsonl` row.
- Every updatable external action must have one `.github/action-pin-upstreams.jsonl` policy row.
- Local actions such as `./path/to/action` are exempt from the action-pin inventory.
- Workflow shell fragments that affect releases, installers, checksums, artifacts, or cross-repo notifications need focused local harness coverage.
- `br` does not perform workflow git operations, releases, pull requests, network dispatches, or upstream lookups automatically. Any live upstream lookup is an explicit operator command.

Branch names in third-party action upstreams are not this repository's integration branch policy. This repository works on `main`; legacy branch mirroring is a push responsibility and must not be reintroduced as a workflow trigger target.

## Inventory Format

`.github/action-pins.jsonl` contains one JSON object per external `uses:` entry:

```json
{"workflow":".github/workflows/ci.yml","action":"actions/checkout","sha":"de0fac2e4500dabe0009e67214ff5f5447ce83dd","tag":"v6.0.2","source":"current workflow ref; inline tag comment"}
```

Required fields:

- `workflow`: repository-relative workflow path under `.github/workflows/`.
- `action`: action owner/name without the `@` ref.
- `sha`: exact 40-character SHA used by the workflow.
- `tag`: reviewed upstream tag or provenance label for humans.
- `source`: short provenance note explaining how the pin was resolved.

`.github/action-pin-upstreams.jsonl` records the allowed upstream policy for update audits:

```json
{"action":"actions/checkout","repo":"https://github.com/actions/checkout.git","latest_allowed_tag":"v6.0.2","latest_allowed_sha":"de0fac2e4500dabe0009e67214ff5f5447ce83dd","source":"current allowed upstream tag"}
```

The verifier checks workflow pins against the inventory. The update audit checks inventory rows against the upstream policy.

## Verifier

Agents should run the verifier's Cargo target through RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_beads_rust_ci_supply cargo test --test workflow_action_pins -- --nocapture
```

Local operators can run the same script directly:

```bash
./scripts/verify-workflow-action-pins.sh
```

The script runs `cargo test --test workflow_action_pins -- --nocapture`. The test fails when:

- a workflow uses an external action without an `@` reference,
- a workflow uses a tag, branch, or short SHA instead of a 40-character SHA,
- a pinned action is missing from `.github/action-pins.jsonl`,
- the inventory SHA disagrees with the workflow SHA,
- the inventory has malformed or stale entries.

Local actions such as `./path/to/action` are ignored by this verifier.

## Update Audit

The update audit is report-only. It reads `.github/action-pins.jsonl` and the configured upstream policy in `.github/action-pin-upstreams.jsonl`, then reports whether each checked-in action pin is up to date, has an allowed update available, points beyond the configured allowed tag, or needs upstream investigation.

```bash
./scripts/audit-workflow-action-pins.sh --format json
./scripts/audit-workflow-action-pins.sh --format text
```

The JSON report includes `action`, `current_tag`, `current_sha`, `latest_allowed_tag`, `latest_allowed_sha`, `status`, and `manual_update_steps` for every inventory row. The script does not edit workflows, rewrite the inventory, create pull requests, push branches, or contact GitHub by default.

Live upstream resolution is explicit:

```bash
./scripts/audit-workflow-action-pins.sh --live --timeout 10 --format json
```

Live mode runs bounded `git ls-remote` lookups for the configured upstream refs and reports `upstream_unreachable` or `missing_tag` instead of mutating files.

Release workflow shell fragments have a separate focused harness:

```bash
./scripts/verify-release-workflow-fragments.sh
```

Agents should run the same test target through RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_beads_rust_ci_supply cargo test --test workflow_release_fragments -- --nocapture
```

That harness parses `.github/workflows/release.yml` and executes the high-risk release fragments against fixtures for reliability override validation, required artifact detection, checksum aggregation, checksum verification, and release-note branch coverage.

The ACFS installer notification workflow also has a focused local harness:

```bash
./scripts/verify-notify-acfs-workflow.sh
```

Agents should run the same test target through RCH:

```bash
rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_beads_rust_ci_supply cargo test --test workflow_notify_acfs -- --nocapture
```

That harness parses `.github/workflows/notify-acfs.yml` and checks the installer checksum, previous-checksum fallback, changed/unchanged comparison, dry-run branch, missing-token notice, repository-dispatch payload, `main` branch trigger, and summary output without sending network notifications.

## Validation Checklist

For workflow changes, record the relevant proof in the commit or bead close reason:

1. `git diff --check`.
2. YAML parser coverage from the relevant Rust workflow test target.
3. `actionlint` on each changed workflow when available.
4. Action pin verifier when any workflow `uses:` entry or action inventory changes.
5. Update audit when reviewing or refreshing action pins.
6. Targeted shell-fragment harnesses for changed release, installer, checksum, artifact, or notification logic.
7. `ubs` on changed workflows, inventories, scripts, tests, docs, and `.beads/issues.jsonl`.
8. Whole-crate `cargo check --all-targets` and `cargo clippy --all-targets -- -D warnings` only when Rust code changed; run them through RCH for agent sessions.

The workflow proof targets are Cargo tests, so agents run those targets directly through RCH. Do not rely on `rch exec -- ./scripts/...` for shell wrappers that call Cargo internally; RCH may classify those wrappers as non-compilation commands.

## Updating A Pin

When changing or adding an external action:

1. Resolve the desired upstream tag or commit yourself, for example with `git ls-remote --tags https://github.com/<owner>/<repo>.git refs/tags/<tag>`.
2. Update `.github/action-pin-upstreams.jsonl` with the reviewed `latest_allowed_tag`, `latest_allowed_sha`, and source note.
3. Run `./scripts/audit-workflow-action-pins.sh --format json` and review the `manual_update_steps` for each affected row.
4. Update the workflow `uses:` entry to the exact 40-character SHA.
5. Update `.github/action-pins.jsonl` with the same workflow path, action name, SHA, tag/provenance label, and source note.
6. Run `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_beads_rust_ci_supply cargo test --test workflow_action_pins -- --nocapture`.
7. For workflow edits, also run `git diff --check`, `actionlint` if available, and the relevant targeted workflow harness such as `./scripts/verify-release-workflow-fragments.sh` or `./scripts/verify-notify-acfs-workflow.sh`.
8. Run `ubs` on the changed workflow, inventory, script, test, and docs files before committing.

This repository's integration branch is `main`. Any legacy branch mirroring is an explicit release/operator responsibility and should not be reintroduced as a workflow trigger target.
