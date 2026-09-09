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

## September 8 continuation — unreleased fixes

The existing historical reconstruction is retained. This bounded continuation
reviews `b330072c..53fbfd41`: five runtime/build fixes, their regression tests,
the documentation corrections, and the intervening plan/tracker records.
Read the actual implementation diffs for 584e9081 (closed claims), 0c6a05f2
(search page selection), 503415c4 (engine-open explanation routing), 7a8aa3e1
(Nix source root), and 1bd33da0 (trusted styling and configuration precedence).
Read 4fe8e47b/3a6bc0d8 for the user-facing documentation corrections. The other
commits record research, plans, or tests; they do not add runtime capabilities.

The September 8 GitHub API recheck still identifies v0.5.11, published at
02:24:24 UTC with 24 assets, as the latest release. These later changes belong
under Unreleased. Do not copy the search commit's one-host timing observations
into a universal performance promise. The stronger eef72c58 claim regression
is undergoing current-source qualification, not a new published guarantee.
Nix package construction and the required Rust gates remain in progress under
i9yzo; no build success or new release is inferred from this changelog update.

### Qualification follow-through at e77ba905 (September 8)

Reviewed the complete e77ba905 diff: three namespace diagnostic messages,
five real engine open-lane probes, the separated positive claim test,
the stable Nix package set/current Darwin SDK interface, test debug profile,
and the exact agent version-example correction from 0.5.10 to 0.5.11.
No runtime behavior is inferred from tracker-only changes. The original
schema-test failure is retained; the full schema target passed after the
single-field correction, without snapshot regeneration.

The four Nix outputs evaluated successfully. Native Linux amd64 construction
produced `/nix/store/1457jm60j6r0bdr9b3xwljvl33r32ssd-beads_rust-0.5.11`,
whose binary SHA-256 is
`001bf43acf4ace6ccc2c08f2960aa1a09b6a86c423cdc98fe2cbaaa15149a951`.
Its 45-step self-test passed with default `self_update` and FrankenSQLite
0.3.18. Nix used the September 8 Fenix nightly; the RCH checks use the pinned
August 31 nightly. Evaluation alone does not establish the other three
Nix builds or seven-target DSR release success.

The same-target all-features CLI sizes were 1,417,694,056 bytes before the
test-profile change and 470,013,824 bytes afterward. The latter retains
`.debug_line`, passed 45 self-test steps, and has SHA-256
`3edf33e2f8ccabf1e0812f03d8bfb282dac29f657668ad8b3196cf65215ce515`.
Neither number is a release-binary budget measurement or a latency claim.
Required whole-target check/Clippy and affected CLI, docs, Markdown, and
MCP tests passed. Common dataset helpers and pinned-BV goldens that returned
early are not credited as exercised coverage. Full current-library and
feature-mode qualification remain in progress; prior timeouts remain failures.

Raw command output and the source manifest are retained under
`/data/tmp/br-i9yzo-20260908-rdczih`; Beads i9yzo comments 1435–1437 and
xmrw6 comment 1436 record the exact test scope, skips, and independent review.
The new changes remain Unreleased; published v0.5.11 assets are unchanged.

### Terminal controls and source qualification at d42ced50 (September 9 UTC)

Reviewed the complete runtime, CLI-reference, and regression-test diff.
Configured text layouts, tracing diagnostics, and human errors now honor
nonempty `NO_COLOR`, `--no-color`, and `TERM=dumb`; empty `NO_COLOR` retains
normal styling. The tests examine raw terminal bytes before normalization,
require retained diagnostics/content, and keep positive colored-output checks.
No golden snapshots were regenerated.

The final source passes strict-RCH all-target/all-feature check and Clippy,
formatting, four affected output targets in debug and release profiles,
365 selected library tests and 67 binary tests. Existing ignored tests,
shared dataset helpers that return early, and zero-run doctests receive no
coverage credit. Earlier whole-library, MCP, search, namespace, and Nix
proofs apply only to their unchanged source surfaces; the Nix construction
proof does not claim the later terminal changes were built by Nix.

The freshly downloaded all-features Linux release-profile binary has SHA-256
`573f4c9d9f2aa39286172fb7ebcc18213de86cad2fc0851a4936374c8c02fe3d`.
Its embedded Git revision is stale worker metadata; source identity instead
comes from matching all 298 Rust/key build files with this commit's code.
It passed 51 claim/output/namespace canary commands, 29 actual PTY commands,
and all 45 self-test steps. An older binary still fails the same terminal
driver on ANSI diagnostics under `NO_COLOR`. The first fresh PTY run found
a fixture title wrapping across table rows; the shorter fixture retains all
byte/content assertions, and both failed and corrected runs are preserved.

Evidence is in the existing qualification root above and
`/data/tmp/br-phm7n-20260908-7sPfyV`; i9yzo comment 1446 records the final
acceptance and bounded solo review. These are source-qualification results,
not seven-platform release or publication receipts. GitHub Actions remain
disabled, and v0.5.11 still has its original 24 assets.

### September 9 — acceptance presence after the v0.5.12 freeze

Reviewed the complete `src/close_policy.rs`, `tests/e2e_errors.rs`, and
CLI-reference diff after frozen source `366c69a63fe18260afc4deff798850053954cac9`.
The new opt-in spelling is `acceptance_criteria_present`; the completion rule
is unchanged. Exact-edge and target-state rules compose. The real CLI matrix
checks prospective text, checked and unchecked checklists, prose, absent or
blank values, both batch orders, fresh comments, and unchanged persisted state
after refusal. Destructive text replacement first encounters the existing
overwrite guard; explicit `--force` still cannot bypass the policy.

Strict RCH passed 111 policy units, 237 CLI error tests, 10 MCP protocol tests,
and all-target/all-feature Clippy and check. The first new test run failed
because it expected the internal comment field name instead of JSON `text`
and overlooked the overwrite guard; the corrected tests retain both guards'
state-preservation assertions. Initial whole-target lint/check attempts hit
the unchanged 300-second cap; documented split runs warmed the same worker,
then whole-target commands passed. Formatting and `git diff --check` pass.
No snapshots, ignored tests, lint suppressions, or time limits changed.

This closes only the presence implementation subtask. Bead g8cib remains open
for a distinct prerequisite field and its CLI/MCP/storage/sync behavior;
7zm00 retains the full proof matrix. The existing MCP suite passed, but does
not itself establish a new presence-specific MCP scenario. These changes do
not belong in the pending v0.5.12 release notes or packaged source.

### September 9 — v0.5.12 publication and distribution

GitHub release 385174315 became public at 02:07:08 UTC, with tag v0.5.12
pointing to `366c69a63fe18260afc4deff798850053954cac9`. The publication did
not move the tag or include the later acceptance-presence implementation.
The release API and independent unauthenticated downloads establish the
complete 24-asset set: seven archives, seven SHA-256 sidecars, seven Minisign
signatures, an aggregate checksum file, and SPDX/CycloneDX source inventories.
Every archive contains exactly its binary, README.md, and LICENSE. All seven
signatures verified with the established release key (ID 36B847D11BA5A0D0).
The source inventories include every one of the 633 Cargo.lock packages;
they are source inventories, not assertions about linked binary dependencies.

DSR v0.1.2 at `87d0be6c6fdf8536decd1663fe2975f13bc9094f` built the artifacts
without Actions, act, or dispatch. The original seven-target run completed
four targets and failed three; its GNU amd64 output also exceeded the intended
glibc floor. Those results remain retained and were not relabeled successful.
Separate GNU and musl runs supplied the four accepted Linux binaries. All
4,169 tracked source files matched the frozen source in every selected build
snapshot. GNU outputs require at most GLIBC 2.28; both musl outputs have no
interpreter, dynamic dependency, or GLIBC references. All seven binaries and
archives fit the unchanged size budgets against both v0.5.10 and v0.5.11.

DSR's strict publisher validated and uploaded its supported 16-asset contract
to a draft. Its first attempt to describe the established 24-asset layout
failed before publication; that failure remains recorded. The operator then
uploaded the eight already-verified signature/aggregate files while the
release was still a draft. Independent verification required the final exact
24 assets before publication and repeated the check on public downloads.
This is not a claim that the older DSR manifest schema accepts every layout.

All seven GitHub-downloaded binaries passed platform CLI canaries and 45
doctor self-test steps. The six Unix targets also passed 29 real PTY checks;
their CLI driver has 51 commands, while Windows has 48. macOS Intel ran under
Rosetta; Linux arm64 ran in native arm64 containers. These checks do not
establish every reconcile invariant or the other three Nix package builds.
Windows retains five existing dead-code warnings; no warning-free Windows
build is claimed.

Crates.io published 0.5.12 at 02:07:38 UTC. Its version API reports an unyanked
3,805,711-byte package with SHA-256
`6d555b0649b2fc85705a71f87019fee5417c5f6daf5a10cd13c60edfd4b23330`, exactly
the separately qualified package. That extracted package compiled with all
features through strict RCH and its binary passed 51 CLI canaries and 45
self-test steps. Publishing used `--no-verify` after that separate compile.
Packaged repository-contract tests are not all green: two AGENTS contract
checks require excluded docs/scripts/tracker files, and two of 235 CLI error
tests require excluded database fixtures. Those failures remain evidence;
they are not credited as passes or masked with new skips.

The public Homebrew formula commit is `1ec62f2`; Scoop is `f054ed8`. Both
repositories had Actions disabled before their pushes. Native Apple Silicon
Homebrew upgraded 0.5.11 to 0.5.12, passed its formula test and all 45 doctor
steps, and retained the exact published binary hash. Both Arch package
architectures built with upstream binary bytes and license preserved; native
x86_64 pacman integrity reported seven files and zero alterations, and 45
doctor steps passed. The aarch64 package has packaging proof only. AUR still
needs an authorized publication credential, so the release bead remains open.

Raw build, publication, installation, checksum, and signature receipts are
retained under `/data/tmp/br-phm7n-20260908-7sPfyV` and the remote paths recorded
in Bead phm7n comments 1461–1465. No registry success is inferred merely from
a manifest version bump, and unfinished post-freeze features remain Unreleased.

Installation follow-through: the public latest installer passed on Linux
amd64 and two separate Apple Silicon machines. Real `br upgrade --json` runs
on isolated copies of 0.5.11 selected public 0.5.12 on Linux amd64 and Apple
Silicon, replaced themselves with the exact published binary, and passed all
45 doctor steps. The initial Mac follow-up used a nonexistent self-test flag
and exited 2; the corrected `--keep` invocation and original failure are
both retained. No installer or product guard was changed to obtain a pass.

Native Windows Scoop installed the public manifest and independently fetched
public archive, with normal checksum verification enabled, then passed all
45 doctor steps with the exact Windows binary. The portable-client bootstrap
emitted a nonterminating missing-shims-directory diagnostic before creating
that directory; it remains in the captured PowerShell error stream. A separate
shim invocation verifies the installed command launches version 0.5.12.

### September 9 — distinct prerequisites and command discovery

Reviewed the complete prerequisite change now committed in
`f9adebc0..b5f3fcac`: model/storage, schema-18 migration, policy, CLI/rendering,
MCP, schemas/docs, and regression tests. The shared tree was committed by
another session during qualification; that did not itself close g8cib or
7zm00. The new field is independent of acceptance criteria. The opt-in
`prerequisites_complete` rule requires a nonempty real checklist with no
unchecked items and composes with presence, completion, and fresh-comment
requirements. Storage evaluates replacements inside the existing atomic
transition preflight. Absent/empty prerequisites preserve the old content
hash; nonempty content participates in hashing, merge equality, and JSONL.

Candidate SHA-256
`058bc11c165b95c46151e260e22eeacc66deae25af9756111928f47f46f269e6`
passed an actual released-0.5.11/schema-17 upgrade, byte-exact database-family
undo, reapply, prerequisite write, and stale-undo refusal. All 119 source,
manifest, lockfile, and toolchain files matched the build worker. A second
reader checked those hashes after the commits: only the separately edited
capabilities file differed. This candidate is a development build, not a
replacement for the immutable v0.5.12 release artifact.

Strict RCH passed 3,097 library tests, with nine pre-existing ignored tests;
205 lifecycle, 239 error, 14 real MCP, 161 schema-migration, 170 sync-safety,
185 reconciliation, 163 capacity-scope, 196 CRUD, and 17 hash-parity tests.
The affected property, snapshot, documentation, and AGENTS-contract targets
also passed. Counts include shared helper tests, not unique independent
scenarios. Whole-target/all-feature check and Clippy passed. This is an
affected-target matrix, not a claim that every release-profile target ran.

Original failures remain retained: cold compiles reached the unchanged RCH
caps; one new clear-field assertion incorrectly expected an empty string
instead of an omitted field; and three snapshots needed the intentional
schema/help additions. Manual snapshot edits were bounded to those additions,
with inverse checks preserving every unrelated byte. A stale MCP executable
initially reported 13 tests after the source contained 14. Matching source
hashes did not prove the executable was fresh: preserved source timestamps
preceded completion of an older compilation. Touching only source metadata
and rerunning produced the named new race and all 14 passes.

Two separately compiled, deliberately incorrect implementations were tested:
vacuous checklist completion, then use of the stored prerequisite value.
Each made the existing CLI regression fail because it admitted a forbidden
replacement (exit 0 instead of 4). Both production files were manually
restored and compared byte for byte with their saved correct versions.
Retained mutant binaries also reproduced prose-only and unchecked-replacement
admission in separate durable workspaces; their receipts identify the invalid
state changes and are negative controls, not product passes. The initial
Cargo test workspaces used the existing temporary-workspace cleanup; the
separate reproductions retain database, command streams, and before/after
state. The first reproduction's Python observer did not explicitly close its
read connection and was slow; the second explicitly closes it before writes.

During command discovery, `capabilities` incorrectly described structured
errors as stderr output and classified gate/capacity commands as unknown and
text-only. Bead bs36u corrects the existing metadata. Its new real workflow
contract test first failed on the old classification, then the full 176-test
schema target passed with actual JSON and TOON gate report/list and capacity
grant/renew/history/revoke commands. An existing real-error test now compares
the published guarantee with observed stdout JSON and stderr plain errors.

Evidence is retained under
`/data/tmp/br-g8cib-prerequisite-20260909-QV5BvoMg`, with the actual migration
workspace at `/data/tmp/br-g8cib-live-upgrade-by_6900g`. UBS reports are not
clean: the capabilities scan reports one existing test-panic critical and
233 warnings; broader prerequisite scans include existing test panics and
SQL/non-secret-comparison heuristics. No blanket scanner pass is claimed.
The final MCP extensions passed all 14 tests: missing/null/empty/whitespace
presence refusals, prose/checked presence handoffs, mixed-checklist refusal,
and unrelated title/comment/dependency preservation through real import.
The first extended run passed 13 and failed one because its assertion omitted
CLI show's `dependency_type` key. The corrected helper checks each surface's
exact keys rather than using a permissive fallback. Both failure workspaces
remain on the worker. Final source check, Clippy, formatting, and diff checks
passed after the last edit. The final MCP test file hash is
`d4ab1c3c2feac494f5e17f7a3e51b553fc74d280eaae547b0941ad3214db3d86`.

An independent bs36u verifier exercised all eight contracts and all six
examples' argument parsing and workspace admission with candidate SHA-256
`4a2ff194921ac5418479d8031be82933eccff6d7bd39f1794e9763b3b7c16267`.
It returned `NOT_INITIALIZED` JSON on stdout with empty stderr from outside
a workspace. Successful mutations are established by the full schema tests;
plain rendering was source-reviewed. The implementation bead g8cib closes
under its original criterion; its separate verification companion 7zm00
retains the outstanding release-profile maintenance run. No overall feature
release or full release-profile suite pass is claimed.
