# Changelog research — 2026-09-08

Requested scope: audit the latest release using `changelog-md-workmanship`.
The existing earlier history is retained; this is not a full-history re-audit.

## Coverage and TODO

- [x] Read project instructions, README, existing v0.5.11 notes, and skill.
- [x] Establish scope: `v0.5.10..v0.5.11` contains 79 commits, including merges;
  `v0.5.11..b330072c91e171f05c2d3414ff7bdd8b8f6f9c04` contains six commits.
- [x] Verify recent release/tag classification and dates against GitHub.
- [x] Research and distill runtime, CLI, and engine changes in the 79-commit range.
- [x] Research and distill testing, benchmark, and workflow changes in that range.
- [x] Separate the six post-tag distribution/evidence commits from released source.
- [x] Link representative commits and exact checked-in Beads records.
- [x] Check claims against diffs, tracker status, and release assets.
- [x] Run the skill validator, verify affected links, and check the final diff.
- [x] Record completion in Beads and deliver the documentation on main and its mirror.

## Evidence spine

- Repository: https://github.com/Dicklesworthstone/beads_rust
- Frozen v0.5.11 source: `b42de9b9aad92d91c926c40a613b73479962e776`.
- GitHub published v0.5.11 at `2026-09-08T02:24:24Z`.
- Initial GitHub release-list query shows v0.5.1 is a published release,
  contradicting the current timeline's combined v0.5.1/v0.5.0 tag-only row.
- Recent release dates mix tag dates with publication dates: v0.5.4 was
  published August 29 UTC and v0.5.2 August 26 UTC. Verify the complete recent
  line before correcting it; explicitly document its date convention.
- Existing v0.5.11 notes repeat connection reuse and sidecar changes and mix
  older v0.5.7-to-v0.5.10 timing measurements into the new release section.

## Boundaries

Use Git diffs first, then tag/release metadata, checked-in `.beads/issues.jsonl`,
and supporting docs. Do not infer completed migrations, contention fairness,
or calibrated performance from a passing release build. Keep AUR publication
open unless there is evidence it was pushed. Never move the frozen release tag.

Research chunks will be recorded here and distilled into `CHANGELOG.md` as
they finish. Earlier version prose outside concrete verified corrections is
outside this update's audit scope.

## Chunk 1 — version spine (distilled)

The complete GitHub release list returned 60 published releases (all non-draft,
non-prerelease). v0.5.1 has 50 assets; v0.5.0, v0.5.8, and v0.5.9 have no
published release. Corrected the recent timeline, corresponding section
headings, and tag links. Release publication dates corrected to UTC:
v0.5.2 August 26; v0.5.4 August 29; milestone v0.4.0 August 23. The v0.5.11
API returns 24 assets: seven archives, seven SHA-256 sidecars, seven Minisign
signatures, an aggregate checksum file, and two SBOMs. The release target is
the frozen b42de9b9 source, not the subsequent packaging commits.

## Chunk 2 — runtime and contracts (distilled)

Inspected the complete range's subjects/file coverage and representative source
diffs: 222dd050/b1dfa462 teardown and grouped-count integration; cb92e944 import
verification and connection reuse; dbcf1ebe close reuse; de4b231b migration
preflight and retry safety; dac8ea99/0a12d6a0/bee3df3e sidecar admission and
engine pins; f81fdc03 MCP policy/publication/resource and init/schema contracts;
829a8357 environment alias; 29a79f9d upgrade tag; e824f4dc/e5e86232 deferral;
bbd1e44f/b6e95891 cache maintenance; 48636625 index diagnostics.

Key corrections: the original summary omitted the MCP URI change and exact
init receipt; blanket retryability was too broad; migration refusal applies
to unsupported core-table layouts, not just duplicate typed edges. The
remaining-budget change in b9b8839e corrects a test's expectation, not the
production polling/deadline algorithm. Do not credit it as a new scheduler.
Earlier v0.5.7-to-v0.5.10 timing observations belong with v0.5.10, not as proof
of a v0.5.11 speedup. Tracker naul5 explicitly retains small-fixture slowdowns.

Source records at published b330072c: 05rjp line 2; ro3m 859; 72j0i 332;
5cxmj 298; naul5 772; azxef.1 482; azxef.2 489; azxef.12 485.
Open boundaries: yyhki 1045, 46zqi 265, zxfz.1 1063. These immutable JSONL
line links avoid unstable links to a changing main-branch tracker.

## Chunk 3 — verification and tooling (distilled)

Reviewed the test/workflow commit inventory, benchmark comparator controls,
conformance refusal/retention diff, concurrent history capture, and source-bound
05rjp coverage notes. f81fdc03 extends independent models and real stdio policy
tests; b9b58dbe fixes model orientation; 9bad2299 adds final quiescent reads;
305687e1 and 51521b96 preserve failures; 3828b602 checks actual init files.
658b62ee/0347b90b/2298d899 add conditional quantile inference, boot identity,
and collection shards. Their commits explicitly leave calibration unfinished.
52ee71c1/954156ac fix benchmark artifact staging/context; no new product
capability is inferred from those plumbing changes.

Earlier workflow fixes are historical evidence only: f25efeff toolchain,
2d8232c5 event concurrency, 20014205 snapshot scope, cb92e944 E2E precompilation,
b9b58dbe close-reason matching/dev E2E, c1525c80 lock allowlist/collation,
58a3e74f and b1dfa462 action-pin updates. Existing ignores and environment
skips remain described (65b44361, 761e782c, e1b8ace0, 9bad2299).

The 149-target count is target coverage, not a count of unique assertions.
Library/binary counts and all ignores are transcribed from 05rjp; this audit
does not rerun or re-certify that historical campaign. Benchmark timings from
088f5b53 are moved into v0.5.10 with their original two-version scope. Closed
workstreams link to their exact b330072c records; open goals remain separate.

## Chunk 4 — post-tag distribution (distilled)

All six commits reviewed: 7858e054 Linux package hashes; 4621308e archive-size
receipts; b70a288d published release/hash/README updates; 60613b20 venue proof
and AUR blocker; 38ccff60 Arch byte preservation; b330072c Homebrew alignment,
nightly-aware documentation, and existing manifest assertions. Source diffs
confirm these follow the b42de9b9 tag; binary changes are not implied.

Live registry check: crates.io 0.5.11 published `2026-09-08T02:24:53.919856Z`,
not yanked, checksum
`11b6b4d808810640fb78685f725eb7f2c258f326d1a38c91f7481cbdec100627`.
GitHub confirms Homebrew commit dbcd7d7f3b532f763f3b52099a000ced06429bfa and
Scoop commit 564dda951de3eb1ee20d9745845da38a05aeeec7. Frozen formula/manifest
links allow inspection of the published package metadata. Bead vq1xl line 980
contains installation receipts and the unresolved AUR authorization boundary.

Initial skill validator passes structural checks with two advisories in
retained earlier history: bare-hash candidates (also matching a Minisign key
ID) and the phrase `and more`. The bounded release rewrite does not erase or
claim to audit every earlier narrative to satisfy those heuristic warnings.
Live-link checks follow separately with the required project HTTP User-Agent;
the skill validator's network mode hard-codes a different User-Agent.

## Final validation

- Read back the completed sections and checked their claims against the
  researched sources. Corrected the draft's ambiguous "ignored probe removed"
  wording: the probe is enabled, not deleted.
- Skill script run directly: structural validation passes; the two earlier
  history advisories above remain disclosed. `git diff --check` passes.
- 90 distinct links checked across current navigation, release/follow-up
  sections, and all added URLs: 89 initially returned HTTP 200. Both HEAD and
  GET returned 404 for the crates.io web page. Replaced it with the official
  version API, which returns HTTP 200 and the exact published version/checksum.
- Every linked release representative (34 distinct commits) belongs to
  `v0.5.10..v0.5.11`; every post-tag representative (five distinct commits)
  belongs to `v0.5.11..b330072c`. The six-commit post-tag inventory also includes
  the evidence-only 4621308e, which needs no separate changelog bullet.
- All 18 Beads link occurrences resolve to their named records at b330072c;
  every record in Completed workstreams is closed. Local doc links exist.
- This change edits documentation and the release bead only. No Cargo suite
  was rerun, no historical test success was relabeled as fresh verification,
  and no GitHub Actions were run.

Delivery: [4b3b7b19](https://github.com/Dicklesworthstone/beads_rust/commit/4b3b7b191f7a0c0b7d5a582d8afe9cf15ed1a37f)
was pushed and both remote branch tips were verified at that commit. The
release tag remains b42de9b9. This final completion record accompanies the
delivered documentation; vq1xl remains blocked only for its unresolved AUR step.
