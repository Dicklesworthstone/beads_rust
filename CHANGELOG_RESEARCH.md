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

### September 9 — class-specific workflow routes

Bead 8dtr0 implements the requested additional workflow edge using the existing
issue type. `workflow.class_transitions` is a list of exact type/from/to rules;
global routes and initial admission remain authoritative. The storage
transaction evaluates the prospective type before required fields, gates,
and capacity. A related storage gap was fixed: an explicit same-status update
must still satisfy a newly loaded strict status vocabulary. Explicit audited
bypass behavior is preserved.

The candidate at SHA-256
`217bdef62139e935f0d727c319dfb66cf398e2a053f7f745d86797d007e8f2b5`
passed 3,110 all-feature library tests (nine pre-existing ignores), 242 CLI
error tests, 18 actual MCP protocol tests, 163 capacity-scope tests, and 161
documentation examples. These target counts include shared helpers. Final
whole-target/all-feature Clippy, check, formatting, and whitespace checks
passed. All 1,155 tracked source/test/build-input files in the recorded
manifest matched the worker after the Clippy-only test correction removed
raw-string delimiter hashes without changing YAML fixture bytes.

An independent reviewer executed 21 fresh CLI calls against the preserved
binary. A matching bug and a task changed into a bug took the additional edge;
an ordinary task used the global planning route. Initial, nonmatching,
unknown-type, prospective bug-to-task, and omitted close-edge refusals kept
the persistent database/WAL/journal/export bytes unchanged. The original
broader file comparison failed on SHM byte 104 (`aReadMark[1]`), which is
retained as a coordination-state observation; whole-family byte identity is
not claimed. This review did not independently rerun the MCP or concurrency
matrix.

Original failures are retained: a default-only policy-field inventory missed
the omitted-when-empty class field, two new MCP close requests used an
unsupported `force` argument, and Clippy found needless raw-string hashes.
Both policy inventory assertions remain intact and now inspect populated
serialization; corrected MCP requests still require the same policy errors
and unchanged state. Two cold dependency checks reached RCH's unchanged
300-second cap before a warmed-worker check passed. An overlapping check was
refused before execution by active-project exclusion; local fallback stayed
disabled. A compiled old-type mutant made the existing storage test fail by
incorrectly admitting a task into `open`; the correct source was restored
byte for byte before the passing suite. No mutant was committed.

Evidence remains in `/data/tmp/br-g8cib-prerequisite-20260909-QV5BvoMg` and
the independent workspace `/data/tmp/br-8dtr0-independent-1xxhp7ya` on ovh-a.
UBS remains non-clean: policy/CLI/MCP source reported two critical findings,
storage 220, and the two integration files 17. Reported samples include
existing test panics, non-secret comparisons, fixture SQL, and a false
security-randomness match on `client.finish()`. No blanket warning clearance
is claimed.

The companion verification now retains successful CLI and MCP observations
through the existing test harnesses. The final CLI run passed all 242 tests
and records both rejected batch orders plus the corrected class and
prerequisite batches, including raw tables, audit rows, and JSONL. The final
MCP run passed all 18 tests, with complete request/response frames, server
stderr, successful class-transition event IDs/actors, and contention loser
projections. All original assertions, barriers, workload counts, and
timeouts remain intact. Final all-target/all-feature check and Clippy,
formatting, and whitespace checks passed. UBS on the three changed test
files reported 28 critical findings, 1,849 warnings, and 242 informational
findings; inspected critical categories were existing test panics, the
`client.finish()` randomness false match, and the fixed Cargo test executable
flagged as an untrusted command. This is not a blanket scanner clearance.

The retained CLI log is `companion-cli-complete-batch-traces.log` (SHA-256
`fac25efc1c29b6013d0db6ce97473d05b7d8665484d232ba2222d4f846bd838a`);
the final MCP log is `companion-cli-mcp-full-traces-final.log` (SHA-256
`98817b956fc369abc998e14073db4cbec71bf77db1ec08036aa21606f284fd95`),
both under the evidence directory above. Their actual CLI bytes match the
independently reviewed `217bdef6...` candidate. The embedded Git SHA in that
cached build predates the dirty class implementation; source manifests and
the executable hash establish identity instead. Earlier incomplete logs
remain retained. The prerequisite companion's frozen `9ac36fa8` release-profile
run subsequently passed 170 sync-safety and 185 reconciliation tests, with no
failures, ignores, or filtered cases. RCH's source-isolation receipt and the
original fixed 1,800-second limit remain in the log
`release-sync-sixth-frozen.log` (SHA-256
`fa09aa4d9303938b71ffdd8071f375b04b32fdc5d7de62fd675414c0480ba1b6`).
The preserved release-profile executable on hz3 is
`/data/tmp/br-7zm00-prerequisite-release-20260909-0435`, SHA-256
`ddcceef81ae9aede43863d9b7722e0e91795c5a147ede40e8ce32f5b6e7aeb68`.
Earlier capped or interrupted attempts remain recorded without pass credit.
These changes remain unreleased and do not complete
the historical migration, lock fairness, or performance calibration work.

### September 9 — typed relationships and the original legacy migration

The schema-19 change makes dependency identity `(source, target, type)` across
storage, CLI/MCP removal, JSONL import/export, and additive reconciliation.
Ambiguous untyped removals refuse before mutation. Exact imported custom type
names take precedence over alias coercion; the remaining relationship keeps
its payload. Canonical schema-18 migration, typed removal, and an operator
foreign-key refusal were independently exercised with candidate
`473f315a6069693493b9c0ce766334b94294a389ad84c650d210d3c45e9abc97`.
The journal is `typed-independent-commands-and-observations.jsonl`, SHA-256
`b5f0d17c408f24f8dd9264a190bf574d8a1be96b7c14f11a18962302cec0f58e`.
An initial whole-family undo assertion failed on the namespace-use sidecar;
the narrower documented DB/WAL/SHM/journal receipt was exact. The stronger
failure and a subsequent observer field-name error remain recorded.

The separate legacy-v15 conversion now admits the exact historical table
profile only after validating declarations, indexes, values, references,
dirty hashes, and child-counter reservations. It stages raw values and row
IDs, preserves parallel and custom relationships without cycle normalization,
and establishes comments/events sequence counters from existing IDs. A
projected witness binds every table, hidden row IDs, and preserved operator
schema. The existing issues rebuild was corrected to retain sparse row IDs.
Unknown operator constraints and incoming references are not silently rebuilt.

The first complete legacy run passed 3,126 library tests with nine existing
ignores; one of 161 migration CLI tests failed because the new fallback hid
the correct canonical-table diagnostic. The assertion stayed unchanged and
dispatch was fixed. A first actual historical apply refused before installation:
all table projections matched, but recreated canonical indexes differed in
`IF NOT EXISTS` and quoting. Only independently attested managed indexes are
now excluded from the operator-schema spelling witness. The original source
database and failed candidate remain retained.

The corrected run passed 3,131 library tests and all 161 migration CLI tests.
Its preserved executable is
`/data/tmp/br-yyhki-legacy-second-ja_bdmh1/br` on ovh-a, SHA-256
`9da9cbb4892bf1a3766bf0b12a1d6d8fb0725cf1679fca21d412d5f667082c4d`.
The 1,203-file source manifest has SHA-256
`b21d6a8b8ae3cb7e2566b714495fa4e58a703b4666277af86b8bc05fda03589c`.
An independent SQLite 3.46.1 observer compared the actual 550-issue source,
successful schema-19 candidate, and undo. Every projected value, storage class,
column, and row ID in all 18 original tables matched, including 465 dependency
rows and sequence values. The three new capacity tables were empty. Eighteen
unreconstructed schema objects, including operator indexes and views, matched
exactly. Undo restored source DB SHA-256
`e9d1b5d9ab67c620cb6128709604da089b88ae1e8cdbe2ddcea0546a42abc205`
and every receipt-covered component. JSONL and the original fixture stayed
unchanged. The complete observer journal has SHA-256
`cd14815339ee8ba7410c68732272747e2a5cdb9e92e804b6eacd12ab2e578928`.

All five original workload failures subsequently passed with explicit
`BR_DATASET_REPLAY` and `BR_DATASET_REPLAY_REASON`: history restore/prune,
label list-all/rename, and the unchanged six-reader, six-iteration concurrency
workload. Their full targets passed 186, 180, and 189 tests respectively.
The original fixture deliberately retains its 550-row DB and 971-line JSONL
starting discrepancy; it was not replaced by a synthetic current-schema
tracker. Explicit replay workspaces are retained even after successful
migration. Shared registry helpers still early-return when their separate
default corpus is absent; those passes are not historical-corpus evidence.
The workload log has SHA-256
`54a801f633e700fca144e75182aada3b61016278481102cc7f0693933a08075d`.

These artifacts live under
`/data/tmp/br-yyhki-typed-20260909-r8QH57VB`. Final qualification is still open:
a subsequent review proved that folding double-quoted text case can hide a
changed SQLite CHECK constraint. That fold was removed and regression tests
added; the executable and workload results above predate this correction.
The first Clippy failure was a redundant visibility qualifier on a private
test fixture, corrected without suppression. UBS remains non-clean; the
two migration files reported 54 critical, 3,039 warning, and 364 informational
findings, including fixture panics/SQL and non-secret-token heuristics.
Release-profile verification is running under unchanged RCH time limits.
Neither yyhki closure nor a complete release-suite pass is claimed here.

The final quoted-text correction was then requalified on executable
`fcd570d76e2d443e59e0542b857ba57c19bec9ad7f15241249b191bc218cc195`,
preserved at `/data/tmp/br-yyhki-legacy-final-mzefvkdn/br` on ovh-a.
All 1,203 source-manifest entries matched the working tree before the final
checks; the manifest SHA-256 is
`ea4a0dbb9fd9693ab6b3d40d8573adc6cad74a16771ec52f796eddaefdef9465`.
The embedded Git SHA comes from a cached build and is not its source identity.
The all-feature matrix passed 3,131 library tests (nine existing ignores),
186 concurrency, 180 history, 189 labels, 22 MCP, 165 migration, 174 sync-safety,
and 189 reconciliation tests. All five original historical workloads executed
and passed; shared harness tests account for repeated per-target counts.
The matrix log SHA-256 is
`a104459a4e153a0af6fec3d6562ad53db198fc56afa9a1d52c3ca14eb4d4b5ca`.
Whole-target all-feature check and Clippy passed, followed by formatting.

The original-source canary was repeated with that final executable: apply took
1.930 seconds and exact receipt-family undo took 0.316 seconds. Independent
SQLite checks again verified all 18 projected tables, raw storage classes and
row IDs, sequence values, three empty capacity tables, foreign keys, and
integrity. The resulting DB SHA-256
`db5ede49f5505a32a452611c86077f3f9e44d9110e670a6d8a6bcf16c3e841ef`
is identical to the earlier candidate whose 18 preserved schema objects were
compared separately. The final observer journal SHA-256 is
`9e1e2946f2d230d96d80adab1d3676c4d7f995d35b602084d46cb62d01c56036`.
These are debug-profile, isolated-copy results; the live tracker is untouched.

The full changed-Rust UBS scan remains non-clean: 490 critical, 18,506 warning,
and 3,124 informational findings. Its log SHA-256 is
`f365e0fc7e7626f82777f0cb4c344b61424c647de458230b95dbe5572d38c854`;
not every warning has been individually triaged. An earlier release-library
run passed 3,051 tests with two existing ignores, but predates the final quoted
text correction and therefore does not qualify the final candidate. The first
full release-suite attempt failed during clean-overlay source transfer before
Cargo; its log is retained as `legacy-full-release-first.log`. The retry and
separate final-source release sync targets are still running. Full release
qualification, fairness, and calibrated performance remain open.

The next release-profile candidate exposed another representability boundary:
legacy dependency types `Review-Custom` and `review-custom` remained distinct
in the converted database but exported to the same normalized JSONL identity.
The source copy and successful-but-colliding export are retained on hz4 at
`/data/tmp/br-legacy-type-case-049yangm`. Admission now refuses any legacy type
whose spelling changes through the interchange model. The negative replay on
ovh-a at `/data/tmp/br-legacy-case-refusal-_bze71mr` returned `CONFIG_ERROR`
before changing the source DB hash. Lowercase custom types remain supported.

With the spelling guard and original fixed polling, release executable
`6b1dfca7489b2a10658ad2cc85776ff383fca87ba70f18510917a8f79bb1beba`, retained
at `/data/tmp/br-fixed-case-final-core-tnz0pnlu/br` on ovh-a, passed 3,051 library
tests (two existing ignores), 66 binary tests, the five original historical
workloads, and the full migration and three sync targets. A fresh independent
SQLite canary verified all 18 table projections and exact receipt-family undo
of the original 550-issue database; apply took 1.909 seconds and undo 0.324
seconds. Its candidate DB hash again matched `db5ede49…e841ef`. Complete
observations are retained beside the executable. The original fixture and
JSONL were unchanged. This executable predates the behavior-preserving helper
extraction used to fix Clippy's function-length warning, so final-source
qualification still requires that newer source to pass.

The separate aged-polling experiment was rejected and reverted. All four
release ABBA runs completed their unchanged eight-stream, 120-second workload
with zero failed or dropped operations and full final-state checks. Although
p95 improved, median latency rose from 135–154 ms to 245–249 ms. A diagnostic
trace still observed a 16.567-second wait while 23 later arrivals acquired the
lock. Those results do not establish fairness or calibrated performance.
Raw comparison and trace evidence remains on ovh-a under
`/data/tmp/br-46zqi-syscall-jwi2sl4x/`.

Qualification remains incomplete: two cold release batches reached RCH's
unchanged 30-minute limit before test execution. Local ENOSPC also truncated
one batch log and prevented a tracker export; the export was subsequently
recovered, and retries use logs in `/tmp`. Missing-`sqlite3` failures in two
sync-artifact tests passed unchanged on a worker with the actual oracle.
The detailed pending targets, source distinctions, and bounded self-review
are recorded in yyhki comment 1500. No test ignores, workload reductions,
timeout increases, GitHub Actions, or new releases were introduced.

The final helper extraction is now qualified. Default release executable
`ad56a8a4c6bfba289f88a09d95f7b0311f9ec1d5aebdca39fa10ee561bca77cc`, retained
at `/data/tmp/br-extracted-helper-release-qcef0br5/br` on ovh-a, matches all
1,203 entries in source manifest `c04f0d2c…fe2846`. Its original-copy replay
passed all 18 table projections, storage classes, row IDs, sequences, foreign
keys, integrity, and exact DB/WAL/SHM/journal undo. The complete observations
have SHA-256 `9aec283766e9c8c805f0efaeb6a13fbdf1cdac05ce6481470eae69ff961f2475`.
The same executable refused the case-distinct legacy types without changing
the source DB. Later edits changed two integration tests and documentation;
the runtime sources remain identical to this executable's inputs.

Release-profile coverage completed through bounded RCH Cargo batches for all
156 integration targets. One target contains only an existing ignored test;
no executed-test credit is assigned to it. The default library passed 3,051
tests with two existing ignores; all features passed 3,131 with nine, and no
default features passed 3,038 with two. The binary's 66 tests, all 22 enabled
MCP protocol tests, and the MCP shutdown test passed. The documentation-test
command succeeded with zero executed tests and ten existing ignores.
The last storage-model target passed all 172 tests independently of a slow
combined batch that reached the unchanged 30-minute cap. Its four remaining
storage targets then passed on the same worker using their compiled binaries.
Two redundant cold fallback builds were cancelled; their logs and cancellation
receipts remain in `/tmp`, and neither receives test credit. Target membership,
failed attempts, and individual proof logs are recorded in the yyhki bead.

The complete suite exposed two obsolete test assumptions. Schema conformance
against pinned Go `bd` 0.46.0 had omitted the previously shipped
`issues.prerequisites` column; it now asserts that exact TEXT/NOT NULL/empty
default declaration and requires the typed dependency key to match. The doctor
chokepoint test had stamped a current database as version 14 while retaining
later columns and keys. Its positive path now derives v14 from the frozen
`d1b90640` schema-15 fixture by removing the v15 gate-history table. The former
mismatched layout remains an explicit no-mutation refusal test. Existing
plan/apply/barrier/undo assertions remain, with byte-exact undo added. Both
corrected targets passed; neither required weakening production admission.

After both test corrections, whole-crate all-feature check and Clippy with
warnings denied passed, as did formatting and whitespace checks. The normal
runtime dependency closure contained no Git-authority libraries. UBS remains
non-clean: its 49 schema SQL findings are all in test fixtures, and the new
integration-file scans report fixture panics and explicit test-binary launches.
This is recorded triage, not a blanket scanner clearance. Qualification used
isolated copies and a fresh solo review; it is not independent human review,
a new release, a starvation fix, or calibrated performance acceptance.

Before landing, `origin/main` advanced to `e64aa62b`. The merge preserves
GitHub #493's operation-neutral policy error and both regression tests, while
retaining the qualified schema implementation and fixed polling. Compared
with the qualified implementation in `542537e5`, the only production change
is the policy error's display prefix. The 156-target suite above remains
pre-merge evidence; it is not a claim that every target was rerun after merging.

The merged default-release executable has SHA-256
`2ef2dde9fb8acb22813c2474ac7a4ec3858b29b1f618be277511bf69b6912a53` and is
retained at `/data/tmp/br-merged-release-t_nqi3hf/br` on ovh-a. All 1,203 source
manifest entries matched. It re-executed the original-copy
migration and exact receipt-family undo, preserving every raw projection
and the original JSONL. The full observation journal has SHA-256
`48dc77b4401eef00f97d966533ba1972f5c6c6d476eb71aaf0f134f96c0d2e5b`.
The case-spelling refusal also preserved the database. Its observer initially
omitted `its` from the expected diagnostic substring; the corrected observer
checked the retained output and hashes without rerunning or changing the CLI.

Merged-source all-feature/all-target check and Clippy passed through strict
RCH. The first check attempt failed at SSH before Cargo started; its retry
passed. Default release passed 3,052 library tests, 66 binary tests and all
12 selected integration targets, including the new policy regression.
The no-default-feature library passed 3,039 tests. Existing ignores remain:
two in each library run and eleven in text conformance. The merged-file UBS
scan remains non-clean; the final changed-code review does not claim a
blanket clearance of existing whole-file findings. Completed logs and the
source manifest are also retained under
`/data/tmp/br-yyhki-typed-20260909-r8QH57VB/`.

The merged all-feature release library subsequently passed 3,132 tests with
nine existing ignores; all 22 MCP protocol tests and the shutdown test passed.
The MCP executable is retained at `/data/tmp/br-merged-mcp-_vjzly5k/br` on hz3
with SHA-256
`08c23c6f12e866f6dbef928affb6e1a72a8f4260228ccfdb1e6a3cd2229ca7eb`.
The all-feature and no-default-feature workers each matched 868 source inputs;
335 historical performance artifacts were absent, with no other missing or
changed input. These runs provide feature coverage, not performance calibration.
The final review and original-data replays were performed by the implementing
agent; they are not independent human verification. No GitHub Actions were used.

The subsequent `origin/main` update to `21becc16` contained byte-identical
Rust sources, tests, manifest, lockfile and build script. Its history was
merged with only this research-note reconciliation; the runtime and test
inputs qualified above did not change.

## 2026-09-09 — Ordered workspace waiters and native Windows recovery

This narrow update covers the landed waiter implementation in `e752c86d`,
the concurrency regressions in `5519f01b`, the unchanged-behavior fast-path
cleanup in `11468bbb`, and native archive exclusions in `b9eff611`.
Git diffs and the `beads_rust-46zqi` issue thread were examined. GitHub's API
confirmed the representative implementation and archive commits are live.
The latest published release remains v0.5.12, published at
2026-09-09T02:07:08Z; these commits are outside that release.

The V3 source manifest contains 1,203 inputs. Its retained release executable
on ovh-a is `/data/tmp/br-46zqi-queue-v3-H9HqM9Mf/br`, SHA-256
`3e672d5403e8f0fda54c73d70537ad903b47f2b04fc2062e5356b8c6e401e523`.
Default release library tests passed 3,058 tests with two existing ignores;
the concurrency target passed all 187. All-feature/all-target check and
Clippy with warnings denied passed through RCH. The preceding V2 sync-safety
batch passed 1,587 test invocations; it is preceding-source evidence, not
a rerun of those nine targets after the two V3 lint fixes. UBS remains
non-clean; no blanket scanner clearance is claimed.

Four eight-stream, 120-second correctness runs used the same preserved
baseline, candidate, and checker on shared ovh-a. All passed the complete
linearizability and final-state oracle, with no failed calls or dropped creates.
The original 100-operation floor and 30-second lock deadline were unchanged.

| Run | Operations | Mutation p50 | Mutation p99 | Longest mutation | CPU per operation |
|---|---:|---:|---:|---:|---:|
| A1 baseline | 1,978 | 493 ms | 3,373 ms | 8,862 ms | 77.59 ms |
| B1 candidate | 1,656 | 845 ms | 1,496 ms | 2,185 ms | 82.09 ms |
| B2 candidate | 1,638 | 850 ms | 1,607 ms | 2,145 ms | 86.00 ms |
| A2 baseline | 1,665 | 476 ms | 5,644 ms | 13,494 ms | 81.96 ms |

CPU per operation divides the checker's total user-plus-system CPU time,
including child processes, setup, and final-state checks, by recorded calls.
It is not an isolated measurement of lock acquisition cost.

The maximum number of Applied peer calls wholly inside a mutation call was
74/1/1/43. These are whole CLI intervals, not measured lock-acquisition
intervals. Candidate tails improved in both pairs, but median calls became
longer, CPU per operation rose about 5–6%, and throughput fell by different
amounts in the two pairs. Shared load and a 37-minute gap between B1 and B2
preclude calibrated performance claims. Neither baseline reproduced the
original starvation timeout, so `beads_rust-46zqi` remains open.

Raw histories, logs, CPU accounting, source identities, and input checksums
are retained under `/tmp/br-46zqi-queue-v3-abba-evidence-20260909/` locally and
`/data/tmp/br-46zqi-queue-v3-H9HqM9Mf/abba-*-120/` on ovh-a. The local
`analysis.json` has SHA-256
`fe4c1c1719bf71668ee3709dfa96e753f5c6c2d41170a66a97ea1b7cfba80b89`.

Native Windows queue and CLI lifecycle checks passed.
Worker repairs restored SSH,
selected the verified MSVC linker, and corrected RCH's recovery rejection of
NTFS's unavailable Unix inode count. The root dispatcher automatically
passed its retained recovery probes and configured `rustc --version` canary
at 18:41:47 UTC. This is recovery evidence, not a project test pass.
The first native release test build exceeded its unchanged 30-minute budget
at 18:23:05 UTC without executing tests. No compiler processes remained when
inspected at 18:55–18:56 UTC; the retry started at 18:58:18 UTC using the
retained cache. The second dispatcher's repaired executable was initially
staged while active jobs finished. No GitHub Actions or new release were used
for this work.

At 19:30:37 UTC the second dispatcher was also upgraded after a successful
drain. Both running executables match SHA-256
`e76d7fafb14fa8dcfcdcb3e06f0913aa92e77be888eff4714bdde3378732608a`;
both original executables were retained. Live capability refreshes confirmed
the installed Windows and Linux toolchains. All 868 required source and
fixture files on Windows matched the V3 manifest. The warm native invocation
had timed out at 19:28:48 UTC; its surviving compiler was left undisturbed
until it exited, and no test executable was found. That attempt remains a
failure, not a native test pass.

The native compiler exposed two unused-import warnings. Imports used only
by Unix tests were moved into those existing test scopes in `doctor.rs` and
the sync command; no test gate, assertion, or runtime behavior changed.
The third native attempt reused the completed dependency cache and reported
neither import warning. Its 13 pre-existing dead-code warnings remain.
The import-only source passed all-feature/all-target Linux `cargo check`
through RCH in 180 seconds after two capped cold/warming attempts and one
startup-inventory refusal. Matching all-feature/all-target Clippy with warnings
denied passed in 231 seconds.
Formatting and whitespace checks passed. The two-file UBS scan remains
non-clean: 77 critical, 4,003 warning, and 1,074 informational findings; the
import-only review is not a blanket clearance of those files.

At 19:52:13 UTC the native Windows invocation completed successfully through
RCH after 13 minutes 10 seconds of release compilation. All three selected
`workspace_waiter` library tests executed and passed; none failed or were
ignored, and 2,517 unrelated tests were filtered. They cover simultaneous
registration, fast-path bypass prevention, timeout cleanup, ordered promotion,
and abandoned waiters. The log is
`/tmp/br-46zqi-windows-imports-native-tests-20260909.log`.
The 41,137,152-byte test executable was retrieved and independently checksummed
at `/tmp/br-46zqi-windows-native-unit-20260909.exe`, SHA-256
`0a4567e593528272cf14b517c75f5c48f093f72e5c5ff2d861f21a94856b5089`.
All 868 required inputs had passed the current-source manifest check before
compilation; its manifest SHA-256 is
`1098fbf692f4abbe182d34eb18c18b5dd7a9bf692ac02281a97e71020caffe64`.

The first two CLI lifecycle dispatches transferred source but refused before
Cargo because the dependency probe exceeded its 20-second SSH budget. Their
failures and refused local fallback are retained in
`/tmp/br-46zqi-windows-native-lifecycle-20260909.log` and
`/tmp/br-46zqi-windows-native-lifecycle-retry-20260909.log`. Read-only direct
manifest probes subsequently passed in 0.66–1.26 seconds. An unrelated native
FrankenTerm build was active and left untouched; its presence alone does not
establish the cause of either timeout.

The third unchanged lifecycle dispatch uploaded source in 9.45 seconds,
verified all 161 dependency preflight entries, and started native Cargo at
20:03:15 UTC. Its log is
`/tmp/br-46zqi-windows-native-lifecycle-retry2-20260909.log`. It reached the
unchanged 1,830-second SSH deadline at 20:33:45 UTC before executing the test;
this is another failed invocation. All 868 required local inputs still match
the source manifest. Before timeout, two native compilers were building the
application library under the owned Cargo process. The worker's 16 GiB RAM
was pressured by concurrent builds; Windows expanded its system-managed
pagefile. No system setting or unrelated process was changed. The surviving
Cargo process continued into the CLI and lifecycle-test targets. By 20:50 UTC
it and its children had exited, and both completed executables were present.
No source transfer or second Cargo build overlapped those surviving compilers.

The normal strict-RCH retry on `wsurf` then passed at 20:51:41 UTC:

```bash
cargo test --locked --release --target x86_64-pc-windows-msvc \
  --test e2e_basic_lifecycle --jobs 1 -- --exact e2e_basic_lifecycle --nocapture
```

It reused the completed artifacts. Exactly one test executed and passed in
9.20 seconds, with zero failures or ignores and 202 unrelated tests filtered.
The complete remote Cargo command took 43.6 seconds. Its eight real CLI calls
covered init, create, update, JSON/text list, JSON/text show, and close; every
call exited zero with empty stderr. The trace names the actual native CLI and
the isolated Windows workspace. The log is
`/tmp/br-46zqi-windows-native-lifecycle-final-20260909.log`.
The original cold-build timeout remains a failure; this passing warm run does
not establish that a cold build fits the dispatch budget on this 16 GiB worker.

The CLI and lifecycle-test executables were retrieved independently of RCH's
zero-file artifact return. Local checksums match the worker's checksums after
the successful run:

| Local artifact | Bytes | SHA-256 |
|---|---:|---|
| `/tmp/br-46zqi-windows-native-cli-20260909.exe` | 22,596,096 | `d0260f099d454ac99b465738fb90c1cdc4ff4e7d94ce90449fda207f560de1e6` |
| `/tmp/br-46zqi-windows-native-lifecycle-20260909.exe` | 24,846,336 | `52ec1fe3a835e1fb752b4ae8164ad8b58e5f608cbbc68ce312485a65bc3984ba` |

The final worker check also verified all 868 required source/fixture inputs;
its log is `/tmp/br-46zqi-windows-final-source-and-binaries-20260909.log`.
Five existing Windows library dead-code warnings and an MSVC linker-message
warning remain in the lifecycle transcript. Native execution is verified;
Windows warning-free qualification and the original starvation acceptance
remain separate from these passing checks.

Before saving this evidence, upstream merge `5427ea14` was integrated. Its
source delta reversed both lint fixes from `11468bbb`: it synthesized
`TryLockError::WouldBlock`, which the retained V2 Clippy log rejects for the
declared MSRV, and restored an equality expression inside `assert!`.
The guarded direct lock attempt and `assert_eq!` were restored manually.
Both versions skip the exclusive fast path while a waiter is registered;
the correction preserves that behavior. All 868 required inputs again match
the exact manifest used for the successful native runs. No new Windows
behavior or new native-build result is claimed for this reconciliation.

The final all-feature/all-target Linux check passed on ovh-a in 289 seconds;
the first selected worker, hz4, had refused admission for stale disk-pressure
telemetry before running Cargo. The first final Clippy attempt on hz3 reached
its unchanged 300-second cold-build cap. The warm retry then passed with
warnings denied in 246 seconds at 21:08:41 UTC. The complete logs are
`/tmp/br-46zqi-final-merge-check-ovh-20260909.log` and
`/tmp/br-46zqi-final-merge-clippy-hz3-warm-20260909.log`.
The scoped final UBS scan remains non-clean: 189 critical, 3,224 warning, and
932 informational findings in `src/sync/mod.rs`. Restoring the already-tested
source does not claim to clear that file's broader scanner findings.
