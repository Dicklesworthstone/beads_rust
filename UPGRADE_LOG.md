# Dependency Upgrade Log

## September 17: FrankenSQLite 0.4.4 uniform family + stable catch-ups (`beads_rust-0edxa`)

- [x] Research the tagged v0.4.4 release (commit `9d3d98778a372aba95d76d05c5c974ac0238c96a`,
  published uniform 0.4.4 family). Confirmed deltas over the previously
  qualified mixed family: native-WAL abandoned-page durable reclamation
  ([`e21d008b4`](https://github.com/Dicklesworthstone/frankensqlite/commit/e21d008b4),
  bd-u2kmg) and INSERT-index-conflict provisional-rowid cleanup
  ([`725e31ee7`](https://github.com/Dicklesworthstone/frankensqlite/commit/725e31ee7),
  bd-55kh5; resolves the vdbe rowid-discard limitation noted in the
  `otrgz` section below). Much of the tagged changelog was already
  present in the 0.4.2/0.4.3/0.4.1/0.4.0 mix. The source review found
  no basis to remove br's opener leases, sole-opener checkpoints, or
  recovery validation; public cross-process MVCC remains incomplete.
- [x] Move all 15 explicit `fsqlite*` manifest floors and all 20 resolved
  engine packages to 0.4.4 (commit `f78c8fe0`). Targeted `cargo update`
  changed exactly the 20 engine records; unrelated edges preserved; one
  Asupersync 0.5.0 across br, engine and FastMCP.
- [x] Catch up remaining direct deps to fresh-verified latest stable:
  thiserror 2.0.20, similar 3.2.0 (insta keeps transitive 2.7.0), libc
  0.2.189. rustix stays pinned `=1.1.4`: fastmcp-client 0.10.0
  exact-pins it on unix (docs.rs source verified); a higher br floor
  deadlocks the resolver, and no fastmcp release permitting newer rustix
  exists. Constraint documented inline in Cargo.toml. All other direct
  deps were already at latest stable (crates.io API re-verified with the
  required User-Agent).
- [x] Security: dropped the stale RUSTSEC-2025-0134 (rustls-pemfile)
  ignore from `.cargo/audit.toml` — the crate vanished from the lock
  when asupersync 0.5.0 / fastmcp-rust 0.10.0 replaced the 0.4.x/0.9.x
  stack. `cargo audit` exits 0, zero vulnerabilities, zero warnings.
- [x] `cargo metadata --locked --offline` consistent; `cargo fmt --check`
  clean.
- [ ] Engine checklist gates on the 0.4.4 tree (RCH, source-content
  receipt, hash `3004952d4a7174ca…`): `--lib`, `model_based_storage`,
  `linearizability_multiprocess`, `repro_avhq`,
  `e2e_schema_migration_upgrade` — run in flight; record results here.
  Two earlier submissions died pre-compilation from RCH worker
  exclusion/SSH resets (no compiler output existed to diagnose).
- [ ] Retained-real-family stress gates 8×60 and 8×90
  (`scripts/br-stress.sh`, source_kind=retained_database_family) plus
  stressed-copy doctor `db.read_only_open_observational`/`db.sidecars`.
- [ ] Final locked all-target check and Clippy, plus all-feature MCP
  protocol qualification. Prior-engine receipts do not qualify 0.4.4.

## In progress: September 16 published engine qualification (`otrgz` / `nx2sh`)

- [x] Recheck the registry instead of continuing to wait on the earlier
  publication inventory. Published facade/core 0.4.2 and pager 0.4.3 contain
  the previously isolated read-only WAL admission fix. Upstream intentionally
  retains unchanged family members at 0.4.0 and requires btree/vdbe 0.4.1.
  Equal patch numbers across every member are not an upstream requirement.
- [x] Independently review archive source correspondence: facade/pager match
  `78134a656`, core/btree/vdbe match `50972bf7c`; core's WAL adapter and vacuum
  source match the earlier `683a241b` candidate. Only pager's archive includes
  VCS metadata; the other provenance claims come from complete source-file
  byte comparisons. This review is not independent runtime qualification.
- [x] Resolve a registry-only candidate through RCH. Preserve the initial
  resolver result, which also reselected unrelated Windows, hashbrown and
  getrandom edges. The corrected candidate changes only five package records,
  keeps those original edges, and retains one Asupersync 0.5.0. Candidate lock
  SHA-256: `665f9fd7ca001beb2bf2f047572ceb4268914feb4904f12ab2990ee66884276a`.
  The qualified candidate is now applied to main's dependency files.
- [x] Run all-target/all-feature check and denied-warning Clippy on the corrected
  candidate. Both passed, followed by 22 MCP protocol tests and one shutdown
  test. Source overlay: `532eea2c00ca349672ebb0d88b47feb1eb2056aeb77ac5ac7ba218c1349a24b6`.
  Logs: `/tmp/br-otrgz-isolated-{check,clippy,mcp}-20260916.log`.
- [x] Run library, model, linearizability, concurrency, observational-open and
  recovery tests on the corrected candidate: 3,161 library passes (nine existing
  ignores), 172 model cases, 25 linearizability cases, 192 concurrency cases,
  166 observational-open cases (one existing ignore) and 167 recovery replay
  cases. The model run includes all 120 generated sequences and the unchanged
  historical 300-issue/264-removal regression. Logs:
  `/tmp/br-otrgz-fair-runtime-20260916.log` and
  `/tmp/br-otrgz-model-startup-tests-20260916.log`.
- [x] Run startup admission and both retained real-family stress gates: 20/20
  startup rounds; eight workers for 60 seconds acknowledged 194 commands with
  52 validation refusals, and 90 seconds acknowledged 305 with 68 validation
  refusals. Every nonzero exit was 4 / `VALIDATION_FAILED`; both independent
  integrity checks passed, DB/JSONL counts matched (1111 and 1134), and no new
  recovery artifacts or unexpected error signatures appeared. Raw archive:
  `/tmp/br-otrgz-published-runtime-evidence-20260916.tar.gz`, SHA-256
  `883290e3dd5d6ca4cf5d8def1537f083504842cb060629407aa3ef0d7c254c78`.
- [x] Adopt qualified pins and correct the engine-version documentation.
- [ ] Continue frozen-source release preparation and all seven native targets.
  Published vdbe 0.4.1 does not include upstream's later `725e31ee7` rowid-discard
  fix; preserve this limitation when assessing test results.

OldSurface is reachable again with 28,821,237,760 bytes free. The retained native
test executable transferred with matching SHA-256 `2bb3ea54c130548dfbb61bc567fa09f26f93cfea4306b39898d0772e7887de54`.
Direct RCH job execution refused because its OS classifier recognizes native
targets only on compilation commands. The normal product Cargo attempt then
timed out downloading dependencies, before tests. A dependency-free Rust launcher
compiled natively through strict RCH and ran the retained verified executable:
all three workspace-waiter tests passed (1.42 seconds) and all five opener-lease
tests passed (87.07 seconds). Logs:
`/tmp/br-native-launcher-{queue,opener}-retry-20260916.log`.
Launcher source: `/data/tmp/br-native-test-launcher-20260916-f5Bcv5`.
These native results qualify source `a42db752` and its original engine, not the
new published engine candidate. Native CLI lifecycle and final release artifacts
remain outstanding. SurfaceBookJE remains below its disk floor; no additional
deletion or admission relaxation occurred.

Concurrent follow-through on existing beads:

- [x] Implement the `46zqi` replenishing-writer regression: seven initial peers,
  a registered victim and seven replacement streams; prove overlapping live
  registrations before resuming the victim, retain each call, and check all 57
  durable comments plus their order. Source review and the full 192-test
  concurrency target passed. Raw archive:
  `/tmp/br-46zqi-replenishment-20260916.tar.gz`.
- [x] Repair `zxfz.1` startup measurement false successes: reject failed setup
  and timed calls, retain both error channels, parse the paginated issue list
  so `show` participates, and add real-command regressions. These changes do
  not qualify cold-cache timing or change performance budgets.
- [x] Finish whole-target check/Clippy for both test changes.
  The first Clippy run caught an overlong test; its durable-result assertions
  were extracted into a helper, without suppressing the lint. Final check and
  Clippy passed on overlay `2c7712a4367795eed49ea21eb11f480f8373d95bf73ae8c4d3ce1154f477ba79`.
- [x] Finish the final startup benchmark regressions: 168 passing cases and
  five existing ignored measurement jobs. Both new real-command controls
  passed; the final concurrency target passed all 192 cases as well.
- [x] Finish all 172 model-based storage cases, including generated sequences
  and the long historical dependency-removal regression (803.18 seconds).
- [x] Tighten retained linearizability evidence: preserve the tested workspace,
  refuse reused artifact directories before collection, and require exact
  JSONL issue count before recording a successful full-state oracle. Source
  review identified and corrected stale-success and extra-row blind spots.
- [ ] Execute the unchanged eight-process, 120-second contention workload on
  the published candidate, with its complete attempted-call history retained.
- [ ] Verify the new smaller real-work benchmark control through RCH. It adds
  opt-in version calls to candidate ready measurements, retains their separate
  outputs and durations, and makes failed extra calls invalidate the sample.
  No measured sensitivity or accepted latency budget is claimed yet.
- [ ] Complete original sustained-contention/native qualification, calibrated
  performance and release obligations before closing their parent beads.

## Follow-through at 04:46 UTC September 16: native capacity still blocked

- [x] Recheck the original worker: 25,058,439,168 bytes free at 04:38 UTC,
  still below the unchanged 25,534,765,261-byte admission floor. No additional
  toolchain deletion authorization was received; none were removed.
- [x] Compress the idle original Cargo registry cache with Windows `compact`.
  It completed with exit zero: 678,228,416 logical bytes stored in 342,421,799
  bytes, a reported saving of 335,806,617 bytes. Log:
  `/tmp/br-native-registry-old-compact-20260916T0439.log`. This is reversible
  filesystem compression of downloaded cache data, not a full per-file checksum
  audit or a successful Cargo test. The second registry cache was not changed.
- [x] Investigate the concurrent roughly 2 GB free-space drop without changing
  system services or databases: no Cargo/rustc process, zero allocated VSS
  storage, 16 GiB allocated pagefile with 65 MiB reported current usage.
  Windows Search's 1,453,490,176-byte database was modified around the drop,
  but one timestamp and a short process-write sample do not prove causation.
- [x] Recheck the remote native test executable: SHA-256 remains
  `2bb3ea54c130548dfbb61bc567fa09f26f93cfea4306b39898d0772e7887de54`, matching
  the preserved local executable. Leave the worker drained with an empty RCH
  queue and no observed Cargo/rustc/compact process.
- [ ] Restore capacity: latest free space is 23,229,202,432 bytes, now
  2,305,562,829 below the floor. The proposed August 4/13/20 toolchain removal
  still requires explicit authorization under AGENTS.md. No admission bypass.
- [ ] Run the retained queue and opener-lease tests through RCH job mode, then
  complete the CLI lifecycle qualification using the pinned original Cargo
  cache. These tasks remain unexecuted; no native qualification or release pass.

## Blocked on native capacity: current-source Windows qualification

- [x] Freeze source `a42db752784e86690956a2518aef00580c4be2d6`; leave
  preserved untracked incident artifacts out of the clean-overlay build.
- [x] Confirm native disk admission (28,272,914,432 bytes free), enable only
  the isolated one-slot SurfaceBookJE worker, and submit the release/default
  feature build through strict RCH with nightly August 31 and MSVC 14.44.
- [ ] Execute the three cross-platform `workspace_waiter` library tests.
  Command uses `--locked --target x86_64-pc-windows-msvc --jobs 1`; keep the
  existing 1,800-second cap. Log `/tmp/br-native-current-queue-20260916.log`.
  First cold-cache attempt ended with RCH-E104 at 02:56:01 UTC September 16:
  SSH timeout after 1,830 seconds including transport grace, outer exit 1,
  no observed tests. Windows cleanup reported no remote PGID; the exact owned
  Cargo process (16756) and compiler child (10696) remained alive. Drain the
  isolated worker while these finish; their post-timeout work is cache warming,
  not qualification. Restore normal disk admission before any fresh attempt.
  By 03:09 UTC both owned processes had exited and the native library-test
  executable existed (42,331,648 bytes); no outcome was captured after timeout.
  Lossless compression preserved all 197 inactive August metadata/symbol hashes
  and all 1,937 idle host-target file hashes, saving approximately 482 MB and
  255 MB respectively. An inactive-library pass added negligible space and
  also preserved hashes. Unattributed concurrent disk growth kept the host below
  admission (24,990,834,688 bytes free). No additional deletion was performed.
  Receipts: `/tmp/br-native-inactive-aug-{before,after}-20260916.sha256`,
  `/tmp/br-native-inactive-aug-rlib-{before,after}-20260916.sha256`, and
  `/tmp/br-native-host-target-{before,after}-20260916.sha256` plus matching
  `compact` logs.
- [x] Complete bounded documentation compression: interrupt the slow full-tree
  pass, verify all 2,804 captured sample hashes in two overlapping checks, then
  compress 3,126 larger files whose complete before/after hashes match. Direct
  short SSH calls succeed where long streaming/xargs calls stall; an oversized
  direct command failed shell parsing and was retained. The selected-file pass
  reports 587,837,394 bytes saved. Receipts are
  `/tmp/br-native-docs-direct-verified-{compact-20260916.log,after-20260916.sha256}`
  and `/tmp/br-native-docs-large-before-20260916.sha256`. This is not verification
  of every file in the interrupted full-tree pass: only the captured sample.
- [x] Correct an operator mistake: attributes 8192/8224 mean not-content-indexed,
  not compressed (the compressed bit is 2048). Earlier claims that the native
  dependency cache and downloaded sources were already compressed were wrong.
  Compress the idle native target: all 3,343 hashes match, 537,872,623 bytes saved.
  Receipts: `/tmp/br-native-target-{before,after}-20260916.sha256` and compact log.
- [x] Preserve hashes while compressing 106 undated-nightly metadata/symbol files
  (225,533,028 bytes saved), 81 unused August 31 Linux/GNU target metadata files
  (162,585,667 bytes saved), and 22 copied database fixtures (94,232,576 bytes
  saved). Stable libraries were already compressed; their hashes also match.
  The August 31 compiler executable and native MSVC target files were untouched.
  Receipts use `/tmp/br-native-{undated,cross-metadata,fixture-db,stable}-...`.
- [x] Retain the unsuccessful warm retry separately:
  `/tmp/br-native-current-queue-warm-20260916.log`. It was admitted at 03:41 UTC
  but clean-overlay creates a nonce-specific root and Cargo download cache.
  Stop the verified owned Cargo PID 13708 before another cold compilation;
  remote and outer exit 127, no tests observed. Do not call this a warm pass.
  The shared target pool survived, but the registry cache did not carry over.
- [x] Retrieve the actual native test executable after the original timeout:
  `/tmp/br-native-a42db752-libtests-20260916.exe`, 42,331,648 bytes, SHA-256
  `2bb3ea54c130548dfbb61bc567fa09f26f93cfea4306b39898d0772e7887de54`.
  Remote/local hashes match; remote Cargo.lock and src/sync/mod.rs also match.
  `/tmp/br-native-current-binary-source-20260916.sha256` binds these receipts.
- [ ] Once capacity is restored, run the retained executable through RCH
  `exec --job` for `workspace_waiter` (three cross-platform cases), then
  `opener_lease` (five cases). Use the tiny frozen canary checkout as the job's
  transfer envelope to avoid another unnecessary full-source copy. The tested
  executable remains the hash-bound a42db752 build, not the canary binary.
- [ ] For the later CLI build, explicitly reuse the original worker Cargo cache
  at `C:/rch/beads_rust/52468a6c306e9db8/.rch-tmp/rch-cargo-cache-surfacebookje`
  and the existing target pool; preserve the frozen source and unchanged cap.
- [x] Leave no observed cargo/rustc/compact process and drain the isolated worker.
  Latest measured free space is 25,057,210,368 bytes, below the unchanged
  25,534,765,261-byte floor; RCH reports critical pressure. About 2.35 GB of
  reported compression savings did not establish durable headroom. Do not
  attribute all concurrent growth to unrelated activity without evidence.
- [ ] Obtain separate authorization before removing any additional toolchains.
  The earlier approval covers only the seven April–July toolchains already
  uninstalled. A proposed next removal is August 4, 13, and 20, retaining stable,
  default nightly, August 25 and the qualified August 31 build toolchain.
- [ ] Execute the current opener-lease regressions on the same native source.
- [ ] Execute exact `e2e_basic_lifecycle`, including real CLI mutations and reads.
- [ ] Bind results to source and executable hashes, retain failed attempts
  and warnings, then drain the isolated worker. These selected tests do not
  establish universal fairness, full-suite release qualification, or artifact
  delivery. Unix-only queue tests are excluded from native proof counts.

## Follow-through: alternate Windows capacity and engine publication

- [x] Recheck all 15 direct engine crates against crates.io. Facade/core/pager
  remain at 0.4.1; the other twelve remain at 0.4.0. No aligned published
  family containing the qualified reader fix is available. Pins stay unchanged.
- [x] Authenticate to `surfacebookje.tail1f21e.ts.net` using its existing
  trusted host identity and fleet key. Direct IP strict checking refused the
  unrecorded alias; the trusted DNS name succeeds without changing host keys.
- [x] Measure alternate native capacity: 7,873,960 KiB free RAM, no observed
  cargo/rustc/link/compact processes, and 12,368,343,040 bytes free on the sole
  510,695,305,216-byte NTFS volume. The unchanged five-percent disk floor is
  25,534,765,261 bytes, leaving about 13.2 GB additional headroom required.
- [x] Inspect older HFDT/wincheck artifacts before attempting compression.
  Of 48 files at least 64 MiB and older than September 4, 44 already have the
  compressed attribute. Only 396,611,412 logical bytes are uncompressed;
  compressing these cannot resolve the capacity deficit. No files changed.
- [x] Complete a bounded lossless compression pass on older April–July
  toolchain binaries/symbols. The initial broad scan timed out after 55 seconds;
  the narrower inventory selected 14 files totaling 1,678,332,416 bytes.
  The normal compressed attribute missed existing WOF compression. The first
  compact invocation only listed files because Git Bash translated `/C`;
  explicit argument-conversion control made the next invocation compress six
  additional symbol files, saving 147,177,472 bytes per compact accounting.
  All 14 before/after SHA-256 values match. The active August 31 toolchain was
  untouched; no files were deleted. Two PowerShell verification wrappers were
  guard-blocked; direct `sha256sum` completed without changing guard settings.
  Receipts: `/tmp/br-native-old-toolchain-before-20260916.json`,
  `/tmp/br-native-old-toolchain-after-sha256-20260916.txt`, and
  `/tmp/br-native-old-toolchain-compact-20260916-v2.log`.
  Latest free space is 15,856,779,264 bytes, still 9,677,985,997 below the floor.
  Other disk activity explains part of the free-space increase; only the
  measured compression savings are attributed to this work. No build pass.
- [ ] Obtain sufficient native disk capacity, then run current-source queue
  and CLI qualification through strict RCH. Successful SSH is not a build pass.
- [x] On explicit user authorization (`yes`, then `confirmed` after the exact
  command and effects were restated), run `rustup toolchain uninstall` on
  SurfaceBookJE for the seven `nightly-2026-{04-22,04-30,06-06,06-07,07-05,07-11,07-20}-x86_64-pc-windows-msvc`
  toolchains. Started 2026-09-16 01:42:41 UTC; exit zero observed by 01:44:45.
  Exact command and authorization are in bead comment 1688; output is
  `/tmp/br-native-approved-uninstall-20260916.log`. Installed-list readback
  confirms all seven absent and August toolchains, stable and undated nightly
  retained. August 31 rustc, clippy and rustfmt remain installed and rustc runs.
  Free disk is 28,285,370,368 bytes, above the unchanged 25,534,765,261 floor.
  First RCH probe overlapped removal and rejected the changing inventory;
  the post-removal probe reports `ok`. Daemon admission and native execution
  still need fresh telemetry; no native test pass is inferred from this cleanup.
- [x] Restart the idle isolated daemon with its existing verified binary and
  unchanged one-job/pressure settings; fresh admission offers one slot. Run
  the unchanged RCH native fixture through strict Windows clean-overlay mode:
  11 passed, zero failed, three existing ignores, remote Cargo and outer exit
  zero at 01:50:23 UTC. Fixture commit
  `10fc2b5b09c9c44063d11d99832ba929c2e237ae`, overlay
  `fe0d5151031be8fda7951fe7fe1f42f7ce344018fdb3ed21e6ada866b230b195`.
  Log: `/tmp/br-native-capacity-clean-canary-20260916-v2.log`.
  Ordinary shared-source dispatch first failed because Git Bash lacks `flock`;
  clean-overlay owns isolated source roots and requires no shared-source lock.
  The first overlay attempt omitted required `--no-overlay` and was rejected
  before dispatch. Retain both failures. Artifact retrieval warned that remote
  tar exited 2 and returned zero files; remote test execution passed, artifact
  delivery is not qualified. Current br queue/lifecycle tests remain pending.
  After the empty-queue check, drain the isolated worker; shared fleet unchanged.
- [ ] Adopt and qualify a suitable published engine family before release.
  This follow-through adds operational evidence, not a shipped capability or
  a reason to close `otrgz`, `46zqi`, or the release bead.

## In progress: 2026-09-15 remaining storage qualification (og86t / otrgz.2)

- [x] Run the four previously unexecuted storage targets through RCH with
  release/all-feature settings on main's current dependency set. CRUD passed
  200 cases, atomic export 176, and storage invariants 193. Workspace failure
  replay passed 164 and failed three; totals include repeated harness cases.
  Log: `/tmp/br-og86t-storage-followup.log`; RCH exit 101. Compilation took
  11m26s; replay ran 191.85s. No tests, timeouts or expectations were changed.
- [x] Trace all three failures to `sidecar_wal_without_shm`: a valid current
  import-generated WAL with its matching SHM archived. Reads fail with
  `DATABASE_ERROR`/`BusyRecovery`; create fails during pending-merge inspection
  with `SYNC_CONFLICT`. The experimental `683a241b` reader fix still requires
  a shared index, so its earlier passing suites do not qualify this case.
  New child `beads_rust-otrgz.2` retains the original positive startup contract;
  neither the engine parent nor the broader sync qualification is closed.
- [x] Exercise existing explicit recovery on a retained private workspace.
  Removing SHM from the active family (archiving it without deletion) made
  `show` fail. `doctor migrate-schema recover` then restored `show`, retaining
  the issue and exact main/WAL bytes. This probe's WAL is header-only (32
  bytes), so it does not establish WAL-only committed-data preservation.
  Log: `/tmp/br-wal-missing-shm-probe.log`; worker receipt:
  `/tmp/br-missing-shm-pokfx09q/receipt.json`; binary SHA-256
  `52e672b8f7d687b7ed10db6bc7471eaa48c4b916c4595fd5fa53dc35f861c6ff`.
  All 1,155 tracked source/test/build-input files in the dispatcher checkout
  match main; inventory SHA-256
  `6f3b1154a0fc54ceca2d60d88736b55a5f45f72a4274c7ca58cfccb896331b3a`.
- [x] Prove recovery with a committed sentinel and pending-merge receipt
  present only in WAL, plus corrupt/mismatched-WAL and live-peer refusals.
- [x] Implement ordinary startup recovery under verified write and sole-opener
  authority, then inspect the recovered pending receipt. Never interpret
  `BusyRecovery` as absence or replace WAL-backed data using JSONL.
- [x] Add strict automatic-recovery WAL validation without changing existing
  tolerant scanner callers. Include engine-generated committed-WAL coverage
  and synthetic header/frame checksum, salt, partial-tail and page-size cases.
- [x] Add CLI regressions for WAL-only rows and legacy pending receipts,
  explicit read-only behavior, live peer exclusion and corrupt-WAL refusal.
- [x] Pass the new unit and CLI cases through RCH; source implementation and
  test construction alone do not establish successful recovery.
- [x] Pass all six focused CLI cases (seven command scenarios). The first
  attempt correctly rejected checkpointed fixture data; setup now uses the
  facade's non-checkpointing drop with the WAL-only assertions unchanged.
  `/tmp/br-otrgz2-startup-tests-v2.log`, overlay
  `f6e171d0f7e332a04ca9e4486e93c84b9c473acf91bda440b292765a547859e3`.
- [x] Identify and fix storage teardown's implicit engine checkpoint: peer
  admission skipped br's explicit checkpoint, but the following engine close
  still copied WAL into main. Use non-checkpointing engine close after the
  existing admitted TRUNCATE. The valid-current-receipt unit test caught this
  with its main-absence assertion; its oracle remains unchanged.
- [x] Requalify the final teardown fix, including the valid WAL-only receipt,
  existing checkpoint tests and storage regressions.
- [x] Run the broad pre-snapshot regression set after restoring tracked source
  fixtures for cached RCH binaries: 4,457 passing target cases, three failures
  and nine existing ignores. The focused three WAL-index unit cases passed,
  including the exact valid pending receipt. Log:
  `/tmp/br-otrgz2-final-runtime.log`. Totals include repeated harness cases.
- [x] Preserve the remaining original replay failure: doctor cannot inspect a
  valid missing-SHM family through the live read-only engine. Implement a
  verified private snapshot fallback, leaving live files untouched. Add
  source-change/symlink refusal tests and positive read-only/status/doctor
  coverage; runtime qualification of this addition remains pending.
- [x] Correct the checkpointed-header health fixture by explicitly checkpointing
  its raw user-version write. It formerly depended on the implicit close
  checkpoint removed by the peer-safety fix. All original assertions remain.
- [x] Rerun the health fixture and unchanged replay with private snapshots.
- [x] Pass the private-snapshot focused cases: three WAL-index units, two
  source-change/symlink units, the health fixture and seven CLI cases. The
  unchanged replay passes all 167 cases; migration passes all 172. CRUD (200),
  atomic export (176) and invariants (193) also pass. Current run:
  `/tmp/br-otrgz2-private-runtime.log`, build overlay
  `2a2c128a4ff9ab7bd8edd5501ef4f18596245cf72a7bbdd93200460743f868e3`.
- [x] Preserve original namespace owner/link-count admission in the private
  copy path. Source review caught that copying creates owned, single-link
  files and could otherwise hide the live source's unsafe topology. Add a
  real hardlink refusal regression, then recheck the final implementation.
- [x] Investigate the remaining live-peer database-busy failure in
  `e2e_sync_flush_only_succeeds_with_large_mixed_prefix_export_hash_rewrite`;
  the unchanged case fails identically on `b41234d0`, before this work. All
  1,156 tracked source/test/build-input hashes matched that revision. The
  remote test returned 101 (one failed, 209 filtered, 6.90s); the outer SSH
  session subsequently ended 255. Log: `/tmp/br-otrgz2-baseline-busy.log`.
  The parent engine blocker remains open; no test expectation was relaxed.
- [x] Complete the broad private-snapshot run: 4,462 target cases passed,
  one pre-existing lifecycle case failed, and nine existing cases remained
  ignored. Library: 3,156 passed. These are target counts including repeated
  harness cases, on the pre-namespace-admission-finalization overlay above.
- [x] Fix initial opener registration's five-second fail-open and serialize
  shared/exclusive transitions. A long recovery must not admit unregistered
  newcomers; concurrent checkpoint attempts must retain peer protection.
- [x] Verify real timeout refusal, concurrent upgrades, protected restoration,
  and successful opening after release. Keep the recovery bead open until
  these safety conditions pass.
- [x] Pass the focused final lease (six), peer checkpoint (one), WAL (three),
  private snapshot (three), structural sync safety (19), doctor index repair
  (one), and missing-SHM CLI (seven) cases on overlay `16a69c65`. The first
  standalone doctor invocation failed because `CARGO_BIN_EXE_br` was unset;
  rerunning with the compiled binary's path passed. Logs:
  `/tmp/br-otrgz-opener-runtime{,-v2}.log`. These are selected cases, not a
  full-suite result. All-target/all-feature check and Clippy also pass.
- [x] Qualify the additional raw doctor paths found by the close audit:
  shared opener registration plus non-checkpointing close for rollback-only
  write probes and partial REINDEX; sole-opener admission for WAL truncation.
  Added real committed-WAL regression with peer and successful final checkpoint.
- [x] Qualify commit-time automatic-checkpoint exclusion in the two raw
  shared-lease doctor paths. Source review caught default engine automatic
  checkpoints during REINDEX, beyond the already-fixed close-time checkpoint.
  Require successful `wal_autocheckpoint=0`; the real regression now creates
  at least 4,000 WAL frames, crossing the engine's urgent adaptive threshold.
  Earlier release build `b8bb3b02` predates this final addition.
  Final overlay `5f43c9a3` passes the large-WAL regression, all 3,161 library
  cases (nine existing ignores), and all 186 release doctor cases (one
  existing ignore), with zero failures. All-target/all-feature check and
  denied-warning Clippy pass. Logs:
  `/tmp/br-otrgz-autocheckpoint-runtime.log` and
  `/tmp/br-otrgz-autocheckpoint-release-runtime.log`.
- [x] Rerun sync filesystem safety after correcting its omitted existing
  `db.fsqlite-migration-state` family member. The first full target passed
  173 and failed one exact file-inventory assertion during import. Added only
  that canonical suffix; runtime JSONL publication paths remain unchanged.
- [x] Close `beads_rust-otrgz.2` after nine RCH release targets pass: 4,618
  target cases, zero failures, ten existing ignores (repeated harness cases
  included). Original replay 167, migration 172, CRUD 200, atomic export 176,
  invariants 193, reconciliation 189, Git safety 174, doctor chokepoint 186,
  library 3,161. Log `/tmp/br-otrgz-release-runtime-pre-auto.log`, overlay
  `b8bb3b025f881e6b38d3fe1dabb6b40f87e40f5bac3ceab680ac6ae235d36ec1`.
  Runtime dependency graph also contains no Git library. Root re-execution,
  not independent runtime verification; bounded honesty audit is bead comment
  1682. This run predates the final doctor automatic-checkpoint change, which
  is separately qualified above; engine and Windows/release blockers stay open.
- [x] Close the separate doctor index-repair bypass (`otrgz.3`): hold the
  corrected exclusive opener guard through checkpoint, REINDEX, close and
  restore; prove peer refusal and later successful repair with WAL-only data.
- [x] Run sync structural/runtime safety checks for the lease changes, plus
  compiler, denied-warning Clippy, focused recovery/doctor and storage tests.
- [x] Pass all-target/all-feature check and denied-warning Clippy, formatting,
  existing migration/recovery/pending-merge tests and unchanged replay tests.
  Recovery overlay `b8bb3b02` is qualified; the later doctor addition remains
  explicitly tracked above.
- [ ] Rerun the unchanged replay target and remaining release gates after
  the recovery fix and published engine update; preserve this failed baseline.

The separate Windows capacity recheck could not authenticate: direct IPv6
and the Mac-dispatcher route timed out during SSH banner exchange; explicit
IPv4 reset during key exchange. No fresh RAM/disk or native-build pass is
claimed. Existing `46zqi` remains open; shared services and host-key checks
were unchanged.

## Completed: 2026-09-15 CLI dependency patches (beads_rust-zdnl9)

- [x] Recheck registry availability. The engine family remains unchanged;
  clap 4.6.7 is current, and clap_complete 4.6.11 supersedes the researched
  4.6.10 patch.
- [x] Update clap/builder/derive together to 4.6.7, preserving all features.
  The lock changes only these three package versions and checksums.
  Upstream deferred command initialization is opt-in and is not enabled.
- [x] Pass the unchanged library, schema and completion targets through RCH:
  3,151 library cases (nine existing ignores), 185 completion-target cases and
  180 schema-target cases. Log `/tmp/br-zdnl9-clap-tests.log`; overlay
  `71c61ba499dc8e148872bf054a929f201d45d3057973fafa3662d2e0d9f6a746`.
- [x] Review the exact published completion 4.6.11 source and update separately.
  Archive/tag revision `2cb76fdf385396f86f37af49784504da560b6e47` fixes static
  Zsh value escaping (upstream #6526); br uses dynamic registration, so that
  fix is not claimed as changed br behavior. The 4.6.10 debug logging changes
  are also included; APIs, selected features and defaults remain compatible.
- [x] Pass completion and CLI regression tests on the final dependency set:
  3,151 library cases (nine existing ignores), 185 completion-target cases and
  180 schema-target cases, all unchanged. Log `/tmp/br-zdnl9-final-tests.log`;
  overlay `d2b0189f13e6b6992f0c70f8677a2d4bb88a25e18d8277ae1465b8aa4966a9e5`.
  Target totals include shared harness tests; they are not counts of distinct
  completion scenarios. Four actual dynamic-completion probes also passed:
  status prefix `in` returns `in_progress`, option prefix `--sta` returns
  `--status` and `--stats`, and nonexistent value/option prefixes return no
  candidates. All exit zero with empty stderr. Log `/tmp/br-zdnl9-dynamic.log`;
  binary SHA-256
  `a40c52afe505a73ca0b0ee828aa22bb00ee8a1e33882605f8c60cfb0a51ff664`.
- [x] Pass all-target/all-feature compiler and denied-warning Clippy checks
  on the same final overlay (`/tmp/br-zdnl9-{check,clippy}.log`). Formatting
  and diff checks pass. The final advisory audit reports zero vulnerabilities
  and warnings (`/tmp/br-zdnl9-audit.json`).
- [x] Review the final diff against the original acceptance criteria. Only
  the four intended package records changed; no application, test, workflow,
  fixture, feature or gate changes. Separate source review informed the update;
  runtime qualification was author-run through RCH, not independently repeated.
  This completes CLI dependency maintenance, not full release qualification.
  Main's existing engine startup blocker remains tracked by `otrgz`/`nx2sh`.

## In progress: 2026-09-15 MCP startup admission (beads_rust-nx2sh / otrgz)

- A standalone RCH runtime probe reproduced the protocol suite's failure:
  start `serve`, immediately run `list --all --json`, then send an actual MCP
  resource request and close stdin. On 20 fresh private workspaces, the current
  release-mode binary succeeded 13 times and exited from the pending-sync
  startup inspection with database busy seven times; the observer succeeded.
  Binary SHA-256:
  `458476c042f02007aa991fc167aa8fe8c156cfde9d8e40950508df161cdb5520`.
  Log: `/tmp/br-nx2sh-startup-current.log`; worker receipts:
  `/tmp/br-nx2sh-startup-24bgspyz/`.
- The previously retained `683a241b` engine candidate succeeded in all 20
  identical probe rounds, including actual MCP responses. Log:
  `/tmp/br-nx2sh-startup-candidate.log`; worker receipts:
  `/tmp/br-nx2sh-startup-w2dztqyr/`. Its SHA-256 remains
  `565d196faf11a63be347b2930d3f30c0131c8ef86e2b4a98d9c0dadd9c887d12`.
  This initial comparison uses different build snapshots; it supports the
  hypothesis but does not isolate an engine-only change.
- Source review found a matching admission defect: fsqlite 0.4.0's read-only
  bootstrap installs an existing WAL then calls `set_journal_mode(Wal)` outside
  its initial-open retry. Pager treats that local adoption as whole-image
  maintenance and requests exclusive authority against peer readers. Candidate
  `683a241b` adopts the mode locally and validates a shared snapshot instead.
  Adding another br-side retry would not correct that exclusive-lock request;
  no retry, startup barrier or test relaxation was added.
- The current-source candidate passed all 3,561 invocations: 3,151 library
  tests (nine existing ignores), 191 concurrency tests, all 22 MCP protocol
  tests, 25 multiprocess linearizability tests and 172 model-based tests.
  This includes all three previously failing MCP startup cases, all 120
  generated model sequences and the 300-issue/264-removal regression. Tests,
  timeouts and assertions were unchanged. The model target took 801.26 seconds;
  the whole RCH job completed successfully within its original 1,800-second
  cap. Log: `/tmp/br-nx2sh-candidate-tests.log`; source overlay:
  `a119686778919ebbd89977960d845e978540bd9adf52451753b8908db3f44a7a`.
  Its isolated lock differs from main in only fsqlite/core/pager;
  FastMCP 0.10.0 and Rustls 0.23.45 remain fixed. All 474 external engine files
  match the prior source inventory (aggregate SHA-256
  `3bf55bcf3b2c6adabc2e91c64684a5e05d368e251122b81c55f974dbcf6be120`).
  Main's manifest and lock remain unchanged. Registry checks still find only
  facade/core/pager 0.4.1, with WAL/types 0.4.0; no aligned release is available.
- Same-current-source startup A/B is complete: main's engine passed 10/20
  rounds, while the candidate passed 20/20. All ten baseline failures were
  pending-sync startup database-busy refusals; every observer succeeded.
  The baseline binary SHA-256 is
  `e9477827f10afc9579e6225b7d27c197bca72834f4a01bfdcf5448611679f0eb`;
  candidate SHA-256 is
  `54869c85911bbcd3944d011e1343bd0e12fe9a6f8cd9a081db25ac0913507a2a`.
  Logs: `/tmp/br-nx2sh-startup-{main,candidate}-current-source.log`.
  Retained worker rounds: `/tmp/br-nx2sh-startup-6bv11mbo/` (baseline) and
  `/tmp/br-nx2sh-startup-q2odd0eq/` (candidate). This comparison fixes br source,
  FastMCP and Rustls versions; it isolates the engine candidate as a whole,
  not an individual upstream hunk. Twenty passing rounds are bounded evidence,
  not a guarantee that every possible startup interleaving is safe.
- Both current-candidate real-family stress gates passed on complete copies
  of the retained recovered/migrated family. Eight workers for 60 seconds:
  160 acknowledged commands, 28 validation refusals (26 closed claims, two
  protected-note updates), database/JSONL both 1,113 records. Eight workers for
  90 seconds: 244 acknowledged commands, 47 validation refusals (42 closed
  claims, five protected-note updates), database/JSONL both 1,133 records.
  Every nonzero result was exit 4 / `VALIDATION_FAILED`. Both runs ended with
  integrity `ok`, zero malformed JSONL lines, doctor errors, new recovery
  artifacts or unexpected error signatures. Source copy witnesses matched.
  Logs: `/tmp/br-nx2sh-candidate-stress{60,90}.log`; worker receipts under
  `/Users/Shared/dsr-sources/br-q93wv-20260912/.rch-tmp/`:
  `br-stress-HXAJAs` and `br-stress-yhH2xN`. Existing recovery history was
  preserved. These integrity stress runs complement, not replace, the
  separate passing linearizability target.
- Follow-on CLI patch research is complete and tracked in `beads_rust-zdnl9`:
  clap 4.6.7 plus its coupled derive/builder packages, then independently
  clap_complete 4.6.10. Existing feature selections should be preserved;
  opt-in deferred initialization is outside dependency maintenance.

### Current qualification checklist

- [x] Reproduce MCP startup failure with actual overlapping CLI and MCP requests.
- [x] Trace read-only WAL adoption and review the upstream admission fix.
- [x] Verify all external candidate source files and isolate its three lock changes.
- [x] Run the unchanged library, concurrency, MCP, linearizability and model suites.
- [x] Finish same-current-source startup A/B and retain both binary hashes.
- [x] Run eight-worker 60- and 90-second stress against complete private copies of the retained migrated family; inspect every nonzero outcome.
- [x] Restore the disposable build checkout's main lockfile after qualification; its SHA-256 matches main (`081e281d9cd228c85919c11bbdbc2e73af5682d7f1698411cff6f28a4b5de30c`). The isolated lock and binaries remain retained separately.
- [ ] Adopt a suitable published engine family and qualify the final dependency pins; keep `otrgz` and `nx2sh` open until their gates pass on main.
- [x] Qualify the researched clap family and completion patches (`zdnl9`).
- [ ] Finish release preparation, cross-platform DSR builds and venue verification on frozen final source; GitHub Actions remain disabled.

## Completed security patch: 2026-09-15 (beads_rust-njkug)

- Updated Rustls 0.23.43→0.23.45 for RUSTSEC-2026-0285. All eight reverse
  dependency constraints permit this patch; the new aws-lc-rs and webpki floors
  already match the lockfile. No engine, runtime, feature or API edits needed.
  The resolver also selected an older `getrandom` for tempfile; that unrelated
  edge was restored to 0.4.3 and full locked Cargo metadata resolution passed.
  The final lockfile diff changes only Rustls version and checksum.
- `cargo audit` now reports zero vulnerabilities and zero warnings against
  advisory database `e2e640471715167f73e22eaf761f2e547adafeec`.
  Logs: `/tmp/br-njkug-{resolve.log,audit.json,metadata.stderr}`.
  RCH all-target/all-feature compiler and denied-warning Clippy checks passed.
  Normal-profile all-feature tests passed 3,151 library cases (nine existing
  ignores), the shutdown case and all ten package-manifest cases. Logs:
  `/tmp/br-njkug-{check,clippy,tests}.log`. All three runs used overlay
  `3df607525f75f553a75aa3d8ae1848a0a1d0e7dc1e46d7e1d532f13660ad9689`.
  The separate MCP startup failures below are not claimed resolved by this
  security patch; this is not full release qualification.
- Final package construction and inspection passed: 745 members, 3,200,357
  bytes, SHA-256
  `f696fa03c2d21987ff435809837687e39123c7702d2df7cf68a351b5f9a266a3`.
  The archive contains patched Rustls, one Asupersync runtime and no Git
  dependencies or local test-evidence artifacts. Construction used
  `--no-verify`; no separate packaged-source build is claimed.
- Bounded self-audit of this work since `d9685fcc`: no implementation, test,
  fixture, snapshot, workflow or gate changes; no new ignores or weakened
  assertions. A separate agent reviewed dependency constraints and the diff;
  runtime execution was author-run through RCH, not independently repeated.
  The failed protocol run, UBS non-result and package-verification limit remain
  recorded. This patch is a release enabler exercised by real tests; tracker
  and research updates are supporting records, not additional capabilities.

## In progress: 2026-09-15 (beads_rust-nx2sh)

- Replaced Git FastMCP 0.9.0 at `180a7c88890705217bb8e202d19555adabf24187`
  with the eight published 0.10.0 crates. The only additional lockfile change
  is the required `dirs` 6.0.0→7.0.0 dependency. Asupersync remains a single
  registry package at 0.5.0, and the lockfile has no Git dependencies.
- Reviewed upstream tag `v0.10.0` at
  `5f1b2ef7155e5823f563fce476112f31ebc3554f` against the exact prior Git pin.
  The new `FinalTaskStore` retention methods affect no br implementation;
  handler/builder APIs and selected ModernOnly protocol defaults are unchanged.
  Upstream fixes cancelled blocking drains and child cancellation reporting.
  A separate source-review agent found no required application changes. Its
  source analysis is not an independent runtime test result.
- Package creation initially included local test-evidence tarballs, producing
  a 96.2 MiB compressed crate. Excluding `tests/artifacts/` reduced this to
  3.1 MiB with 745 members; archive inspection confirmed registry-only FastMCP
  metadata and retained source/build inputs. No evidence files were deleted.
  Logs: `/tmp/br-nx2sh-package{,-clean}.log`. These commands used `--no-verify`;
  they establish package construction, not compilation of the packaged source.
- RCH all-target/all-feature check and denied-warning Clippy passed.
  Release/all-feature library tests passed 3,151 cases with nine existing
  ignores. MCP protocol tests passed 19 and failed three; the separate shutdown
  test and all ten package-manifest tests passed. Logs:
  `/tmp/br-nx2sh-{check,clippy,tests,shutdown,manifest-tests}.log`.
  The three failures exit from main's pending-sync startup inspection with
  database busy, before dispatch reaches FastMCP. Their immediate CLI/raw
  observations overlap server startup. This is consistent with the existing
  engine read-admission issue, but the exact engine cause is not isolated.
  The tests remain unchanged and full qualification stays open.
  The existing shutdown test covers idle server interruption and reopenability,
  not cancellation during an active handler. No tests were weakened or added
  merely to mirror the dependency edit.
- Fresh `cargo audit` found the pre-existing `rustls` 0.23.43 vulnerability
  RUSTSEC-2026-0285 (patched in 0.23.45). The separate qualified security patch
  above closes `beads_rust-njkug`; the initial failed audit remains at
  `/tmp/br-nx2sh-audit.json`.
  UBS on the two changed manifest files exited 2 without scanning Rust source;
  it is not a clean scanner result. Log: `/tmp/br-nx2sh-ubs.log`.

## Completed recovery capability: 2026-09-15 (beads_rust-otrgz.1)

- `br doctor migrate-schema recover` now performs explicit engine admission
  recovery using the unchanged main dependency pins. It requires exclusive
  opener and write authority, backs up all nine engine-family components,
  rehearses on a complete private copy, binds the live open to retained VFS
  identity, and verifies protected main/WAL/journal bytes and logical contents.
  It records prepared, complete or failed receipts and leaves schema migration
  as a separate operation. Failure retains the complete prestate backup.
- RCH all-target/all-feature check and denied-warning Clippy passed. The final
  release/all-feature run passed 3,151 library tests with nine existing ignores
  and all 165 schema-migration end-to-end tests. Coverage includes committed WAL
  rows, empty-WAL recovery followed by migration, peer refusal, symlink refusal,
  failed private recovery and database-identity replacement. Overlay:
  `0cd8eac713e217ced6b10cae21894d967cec64f13ed39afab36bf16fceeb41fe`.
  Logs: `/tmp/br-otrgz1-{check,clippy,final-tests}.log`.
- The actual CLI recovered a complete 33-file private copy of the retained
  schema-17 family. Planning initially failed with `BusyRecovery`, recovery
  completed, and planning became eligible with all 1,086 issues preserved.
  The original source hashes, inodes, modification times and change times
  remained identical. Worker receipts: `/tmp/br-otrgz1-cli-0q6lb0tk/`;
  controller log: `/tmp/br-otrgz1-cli-canary.log`. Binary SHA-256:
  `8d269b15c44f07727279d680168f7f025c50a21d8aba95d3f29ac9ce077a3bc9`.
- Formatting and diff checks passed. UBS whole-file scanning exited 1; reviewed
  critical reports were test assertions, an existing temporary-name nonce and
  comparisons of non-secret recovery witnesses. No tests were weakened or
  scanner findings suppressed. This closes the recovery capability, not the
  parent engine qualification or release gates.
- A fresh official-registry inventory found published FastMCP 0.10.0, which
  requires the existing asupersync 0.5.0 and may remove the Git-only publishing
  blocker. Its custom task-store trait changes have no implementation in br.
  Qualification remains open in `beads_rust-nx2sh`, including clap 4.6.7 and
  clap_complete 4.6.10 patch review. The fsqlite family still lacks an aligned
  published update. No dependency pins changed in this recovery work.

## In progress: 2026-09-14 (beads_rust-otrgz)

- The registry still has no newer published engine patch. An isolated candidate
  uses published 0.4.1 core/pager packages with exactly the four source files
  changed by upstream `683a241bc3830a500bfcb8f9e5380e57fdebcf9c`. Git blob IDs
  match the upstream patch, and all 474 staged dependency files match the
  worker copy. The normalized published manifests are retained. Main's
  `Cargo.toml` and `Cargo.lock` remain unchanged.
- The private lockfile replaces only fsqlite/core/pager relative to main.
  Core and pager use isolated path dependencies. Initial resolution attempts
  preceded a complete transfer and failed; a complete copy and hash comparison
  preceded successful resolution (`/tmp/br-otrgz-683-resolve3.log`).
- RCH release/all-feature concurrency passed all 25 tests, with 297 operations
  and zero failed calls in the eight-process workload. Binary SHA-256:
  `565d196faf11a63be347b2930d3f30c0131c8ef86e2b4a98d9c0dadd9c887d12`;
  retained on ts2 as `/tmp/br-otrgz-683-br`. Log:
  `/tmp/br-otrgz-683-concurrency.log`. The br overlay fingerprint is
  `3af58ea824e608cd2f8b09c174004fc2da9e7b6186279e258d7ee803056d454a`;
  external path-dependency hashes are recorded separately in
  `/tmp/br-otrgz-683-{source,worker}-hashes.json`.
- The original real-family migration preflight still fails with `BusyRecovery`
  (exit 2), so no migration or stress ran. Evidence:
  `/tmp/br-otrgz-683-migration.log`; preserved worker copy:
  `/tmp/br-otrgz-real-family-683/migrated/`. The unpublished follow-up does not
  resolve this blocker. The candidate remains diagnostic, not a release bump.
- The retained SHM header has `is_init=1`, page size zero, no frames/pages and
  zero salts; the database and WAL use 4096-byte pages and WAL salts are nonzero.
  `SharedWalIndexHeader::validate` rejects the zero page size. Pager reader
  admission reports `SharedHeaderInvalid` before reaching the new empty-WAL
  slot logic. The remaining problem needs explicit engine recovery under the
  correct database-family authority; no sidecars or validation were bypassed.
- A direct probe using main's unchanged 0.4.0 pins now proves that the engine's
  existing writable recovery can admit this legacy family. On a fresh complete
  copy, the initial read-only open returned `BusyRecovery`; a writable
  existing-only open changed only `beads.db-shm`, reported integrity `ok`, and
  found schema 17 with all 1,086 issues. The following read-only open changed no
  bytes, and normal migration planning became eligible. The original snapshot's
  hashes, inodes, modification times and change times remained identical.
  Worker evidence: `/tmp/br-otrgz-recovery-probe-5l1y5div/receipt.json` and
  per-step outputs; controller log: `/tmp/br-otrgz-recovery-probe-run.log`.
  Probe SHA-256:
  `8914cce67f40348a1865aaa3d158a584e9dcd884ab95cfec1127fe2f334e9df1`.
  RCH compilation succeeded; artifact delivery reported E327/exit 102 because
  the Linux binaries are foreign to the Mac dispatcher. The diagnostic then
  ran successfully on the Linux worker through an RCH job.
- On another complete copy of that recovered family, the retained `683a241b`
  candidate completed migration 17→19 and `ready`. Doctor reported zero error
  findings: integrity, read-only observation, sidecars and DB/JSONL counts
  passed. Its exit 1 records warnings, including retained historical recovery
  artifacts; the helper therefore stopped after doctor rather than reporting
  an entirely clean run. Family and full receipts remain at
  `/tmp/br-otrgz-migrated-recovered-luevu8q_/`; log:
  `/tmp/br-otrgz-migrate-recovered.log`. This proves recovery plus migration on
  private copies, not an implemented live recovery command in br.
- Both required retained-family stress gates now pass on that experimental
  candidate through RCH. Eight workers × 60 seconds produced 194 acknowledged
  commands and 37 nonzero outcomes, ending with 1,120 DB/JSONL records; eight
  workers × 90 seconds produced 295 acknowledged commands and 58 nonzero
  outcomes, ending with 1,143 records. Both retained clean integrity, zero bad
  JSONL lines, zero doctor errors, zero new recovery artifacts, and zero
  unexpected error signatures. The 60-second nonzero outcomes were 30 closed
  claims, two already-assigned claims, and five protected note overwrites.
  The 90-second outcomes were 53 closed claims and five protected note
  overwrites; all 95 nonzero commands returned `VALIDATION_FAILED`/exit 4.
  Logs: `/tmp/br-otrgz-recovered-stress60.log` and
  `/tmp/br-otrgz-recovered-stress90.log`. Full command receipts and families are
  retained on ts2 below
  `/Users/Shared/dsr-sources/br-q93wv-20260912/.rch-tmp/` in
  `br-stress-h6kN3F` and `br-stress-NIBKwd`, respectively.
- Remaining: implement guarded live recovery (`beads_rust-otrgz.1`), complete
  the candidate's remaining engine/suite gates, and resolve the published
  engine-family and FastMCP dependency constraints before release. These
  successful experiments do not qualify unchanged main's 0.4.0 engine.

## In progress: 2026-09-13 (beads_rust-otrgz)

- Investigate the published FrankenSQLite 0.4.1 patch against 0.4.0.
  The 0.4.0 release concurrency gate completed only nine operations in its
  30-second, eight-process workload (minimum 100); an isolated rerun completed
  18. Both failed. The planted-success-without-writing negative control also
  failed during setup. These failures block engine qualification.
- A retained syscall trace shows two read-only CLI opens holding shared database
  locks while competing for exclusive maintenance: one owns RESERVED/PENDING
  and waits for the shared range, while the other retains a shared lock and
  waits for RESERVED. Disabling the read-only fast-open path did not make the
  original gate pass. Evidence is retained under `/tmp/br-otrgz-strace/` and
  `/tmp/br-og86t-linearizability-failed/`; no timeout or throughput assertion
  was weakened.
- Published 0.4.1 records source revision
  `a8b76fb810ff0e26bb49c81add43c7709e1e7302`. Its parent
  [4f073081](https://github.com/Dicklesworthstone/frankensqlite/commit/4f0730819d6bfdc7aca813a51b8e3ea88a365395)
  lets a read-only constructor adopt an already validated WAL without exclusive
  maintenance. The later
  [683a241b](https://github.com/Dicklesworthstone/frankensqlite/commit/683a241bc3830a500bfcb8f9e5380e57fdebcf9c)
  changes another read-only WAL binding path and is **not** in 0.4.1. The exact
  retained concurrency gate must establish whether the published patch suffices.
- Registry inspection found 0.4.1 releases only for `fsqlite`, `fsqlite-core`
  and `fsqlite-pager`; the other twelve direct engine crates remain at 0.4.0.
  A synchronized 0.4.1 manifest fails resolution. The main checkout retains
  its original manifest and lockfile. An isolated release checkout tests the
  published combination for diagnosis; it is not an approved dependency update.
- Experimental lockfile resolution changed only those three crates and their
  checksums. Core/pager archive hashes match the registry lockfile. The original
  eight-process, 30-second gate passed through RCH with default settings:
  **305 operations, zero failed calls**, 28 issues checked against the model and
  published JSONL. Receipt: `/tmp/br-otrgz-published-041-concurrency.log`,
  overlay `443c04c8dcece30a7752580dbc4ae76ac7247f3a0f4231f5614492d190981b4a`.
- The complete concurrency target also passed: **25 tests, zero failures**,
  including the planted-liar negative control; its workload completed 304
  operations with zero failed calls. Evidence:
  `/tmp/br-otrgz-published-041-full-concurrency.log`. Retained candidate binary
  on ts2: `/tmp/br-otrgz-published-041-br`, SHA-256
  `baaba3a90b9e38062655ea08ebae14d58dc92ef5841900415057486b7952593f`.
- Pending: run storage/model gates;
  qualify migration and 60/90-second real-family stress; complete compiler,
  Clippy, formatting and remaining release tests. Prior 0.4.0 results do not
  qualify 0.4.1. No new release has been published.
- The retained real schema-17 family still fails `doctor migrate-schema plan`
  with `DATABASE_ERROR` / `BusyRecovery` on the isolated 0.4.1 binary (exit 2).
  No migration or stress workload ran. The fresh failed copy and command
  receipts remain on ts2 under `/tmp/br-otrgz-real-family-041/migrated/`;
  RCH log: `/tmp/br-otrgz-041-migration.log`. Concurrency success alone does
  not resolve this separate release gate.
- A traced rerun shows no failed lock acquisitions: namespace, database and
  SHM read locks all succeed before the immediate refusal. The source has a
  32-byte WAL header with no frames and a 32,768-byte SHM file. This narrows
  the next investigation to read-only empty-WAL admission, including the
  unpublished `683a241b` lease changes; it does not prove that commit fixes
  this family. Retained trace log: `/tmp/br-otrgz-041-migration-traced.log`;
  worker syscall traces: `/tmp/br-otrgz-041-migration-trace.*`.

## In progress: 2026-09-11 (beads_rust-4e2n1)

**Release outcome (2026-09-12):** v0.6.0 is published on GitHub and crates.io;
Homebrew and Scoop updates are installed and verified. Both Arch packages are
built, but AUR publication remains blocked by SSH authentication. The release
bead stays open for that obligation. The chronological evidence below retains
earlier pending states and failed attempts.

**AUR follow-up:** live RPC lookup on 2026-09-12 confirms `br-bin` is not
registered. The existing package for this repository is
[`beads-rust-bin`](https://aur.archlinux.org/packages/beads-rust-bin), version
0.2.7-1, maintained by `sQVe`. Our `br-bin` recipe and built packages are local
artifacts, not an update already accepted by that maintainer. The configured
controller SSH identity still receives `Permission denied (publickey)`;
neither controller nor Mac has an AUR-specific identity configured. Completion
needs the authorized publishing account/host/key and confirmation of the
intended package identity. The user has been asked for this missing information.
No new key, duplicate package, account change, or maintainer message was sent.
RPC evidence: `aur-package-identity.json` in the retained evidence directory.

- GitHub release `387447968`, published 04:05:02 UTC, has exactly 24 assets.
  All draft and unauthenticated public downloads passed size/hash/signature/
  payload checks; each draft-downloaded binary passed its CLI/migration canary.
  A first GNU arm64 invocation targeted a stopped retained container and never
  ran; the replacement invocation passed in the live native Linux container.
- Four strict DSR snapshots match all 4,169 frozen tracked entries; all seven
  successful RCH receipts match the source, including all 118 required build
  inputs. Witness SHA-256:
  `0d5eacac423f95ccb456fcaece23818a81072add7d04d0770c30cdf2572dbf53`.
  Seven archives satisfy the unchanged 0.5.10/0.5.11 size budgets and carry
  signatures from br key `36B847D11BA5A0D0`.
- crates.io published at 04:05:43 UTC with registry checksum
  `0d0fbac9a6c83b1ee48ab585f3d5f3fe9c8005a05cab7e039502d9293ed1cecf`,
  exactly matching the separately qualified upload payload.
- Homebrew tap `e6ee63a` and Scoop bucket `b311b12` are live. A concurrent
  unrelated Homebrew formula addition was merged before the push; no force
  update was used. Homebrew's real upgrade/test and Scoop's real shim install
  passed; installed hashes match, and each passed all 45 doctor steps.
- Public installers and actual 0.5.12-to-0.6.0 `br upgrade` passed on Linux
  amd64 and Apple Silicon. All four installed/upgraded binaries match release
  hashes and passed 45 doctor steps each. Both Arch package payloads preserve
  the accepted binaries; amd64 `pacman -Qkk` reports seven files, zero altered,
  and its installed doctor passed 45 steps. ARM Arch installation was not run.
- Final three-manifest overlay passed ten `package_manifests` tests through
  RCH, zero failures/ignores. Evidence and ready-to-publish Arch packages are
  retained in `/tmp/br-4e2n1-evidence-20260912/`; canonical archives and original
  DSR run records remain on the Mac under this campaign's private paths.

- Registry inventory checked against `Cargo.lock`: all direct dependencies already resolve to latest stable except `fastmcp-rust` 0.8.1 → 0.9.0, `asupersync` 0.4.9 → 0.4.11, and `toml` 1.1.4 → 1.1.6. FrankenSQLite remains current at 0.3.18; no engine bump is needed.
- Published FastMCP 0.9.0 requires `asupersync =0.4.10` and its client requires `toml =1.1.5`. These three manifest changes are one indivisible compatibility update: changing any one independently makes Cargo resolution fail. Newer asupersync 0.4.11 and TOML 1.1.6 remain excluded by these upstream exact pins.
- Research: [FastMCP published dependencies](https://crates.io/api/v1/crates/fastmcp-rust/0.9.0/dependencies), [client dependencies](https://crates.io/api/v1/crates/fastmcp-client/0.9.0/dependencies), [v0.9.0 changelog](https://github.com/Dicklesworthstone/fastmcp_rust/blob/v0.9.0/CHANGELOG.md). Stable tag `fd440f3d361a58e5578c02f306e0dcf80cf8e479` preserves the caller-owned-context transport API br uses. Removed context-free stdio and synchronous HTTP APIs are not used here; feature selection remains unchanged. Runtime `current_thread`, `block_on`, and `request_cx_with_budget` bodies are unchanged in [asupersync 0.4.10](https://github.com/Dicklesworthstone/asupersync/blob/v0.4.10/src/runtime/builder.rs); scheduler changes still require storage regression coverage. [TOML 1.1.5](https://github.com/toml-rs/toml/blob/toml-v1.1.6/crates/toml/CHANGELOG.md) fixes owned integer/float conversion.
- Baseline: `a22c251b`; `cargo test --locked --lib --all-features` through RCH on ovh-a passed 3,138 tests, with zero failures and nine existing ignores (2026-09-12 00:51 UTC). Remote process and outer command exited zero. Controller disk exhaustion prevented durable lease receipt updates; the remote test result remains observed, but no persisted lease receipt is claimed.
- Resolver updated the eight FastMCP crates, asupersync, TOML and `syn` 3.0.3 → 3.0.5. [FastMCP derive](https://crates.io/api/v1/crates/fastmcp-derive/0.9.0/dependencies) pins syn exactly; its patch changes foreign `safe fn` parsing and lexical-error spans. Cargo also reselected already-present compatible edges: gix-imara-diff's hashbrown 0.17.1 → 0.16.1 (`>=0.15, <=0.17`) and tempfile's getrandom 0.4.3 → 0.3.4 (`>=0.3.0, <0.5`). These edge changes are included in candidate qualification.
- Security audit against freshly cloned RustSec revision `b50980aad8b8f14f77e25a97b32dd94bf008b0af` reports zero vulnerabilities across 633 locked packages, including a second run with no project exclusions. That second run reports only the existing unmaintained `rustls-pemfile` notice (RUSTSEC-2025-0134). Its documented exception remains; removed the obsolete gix-date RUSTSEC-2025-0140 exception because the resolved graph no longer triggers it. Receipts: `/tmp/br-4e2n1-evidence-20260912/dependency-audit*.json`.
- Controller root filesystem filled during concurrent work. A separate checkout at `/tmp/br-4e2n1-release-20260912` preserves the pending update without deleting or reverting the original checkout. Candidate tests still use explicit Cargo manifest/lockfile overlays on the frozen original base, through RCH.
- Candidate library gate passed through strict RCH on ovh-a: 3,138 passed, zero failed, nine existing ignores; 48.09 seconds of test execution, completed 2026-09-12 01:06 UTC. Outer command exited zero and returned clean-overlay fingerprint `8193c7e1641641bae241ac6c2fc3d59c02c546359c04163cc54ff4b3d9e9ddd2`. MCP protocol/shutdown and package-manifest integration qualification is running next.
- Release infrastructure: dedicated RCH daemons on Mac (PID 81765) and ts2 (PID 95603), each with only the opposite host as a worker, passed live capability probes. Linux/GNU Windows DSR commands will launch on Mac and compile on ts2; Darwin commands will launch on ts2 and compile on Mac. Source-derived routing is accepted by pinned DSR `87d0be6c`; actual DSR build/artifact round trips remain unproved. Configs and probes are retained in `/tmp/br-4e2n1-dsr-rch/` and `/tmp/br-4e2n1-evidence-20260912/`; no shared daemon/config changes or local builds.
- Candidate MCP/package integration gate passed through RCH at 2026-09-12 01:13 UTC: 22 protocol tests, one shutdown test, and ten package-manifest tests; zero failures or ignores. Compilation took 6m24s. The 38-target `e2e-a-l` shard is next. This RCH frontend rejects combining `--job` with `--clean-overlay`, so the shard's existing membership is expanded into direct Cargo `--test` arguments to preserve the frozen-source overlay contract.
- The release canary now exercises deferred-claim refusal, distinct prerequisites and acceptance criteria, prospective class transitions, parallel typed dependencies and bounded ready output. Its Linux harness qualification passed 82 commands against the all-feature development binary; this is not release-binary qualification. The initial harness expected the wrong error code for a prohibited workflow edge (`POLICY_VIOLATION` instead of the existing `VALIDATION_FAILED`); corrected that assertion and reran in a new retained workspace. Evidence: `/tmp/br-4e2n1-evidence-20260912/canary-harness-qualification-v2.log`.
- Full `e2e-a-l` shard passed at 2026-09-12 01:26 UTC: all 38 Cargo test targets, 6,521 test invocations, zero failures and four existing ignores, outer exit zero. The final manifest-comment correction changes the clean-overlay fingerprint to `dee6fc39c65b8d7c73d953080cfde6fc18be34f76eaa9f49f59de156ea1d3823`. The 39-target `e2e-m-z` shard is running on ovh-a; the independent 50-target storage shard is compiling on ts2.
- Full `e2e-m-z` shard passed at 2026-09-12 01:37 UTC: all 39 targets, 6,521 test invocations, zero failures and two existing ignores, outer exit zero. The 22-target miscellaneous shard now runs with the pinned reference tools explicitly on PATH and `BD_BINARY` set. A dedicated Mac qualification commit `4de8920fc13e4281690dd5369b53970cc5ae12b7` preserves all 4,169 tracked entries from `a22c251b` with only the two candidate manifest/lockfile changes; exact Git blob/mode comparison reports zero other differences. This temporary test commit is not the release source or a published commit.
- Conformance prerequisites: ovh-a's installed bd 0.40.0 and bv 0.24.1 differ from the test pins. Downloaded bd 0.46.0 and bv 0.22.0 from their published GitHub releases, verified their archive SHA-256 sidecars and the copied executable hashes, and placed them in a dedicated `/tmp/br-4e2n1-reference-bin` directory without replacing installed tools. Executable hashes: bd `8993761b844c84f76d0128efcd237adaf2f0358fec2eae8337125c881f1c0565`; bv `70a534fc61e928918b08e7812cc2419c56da5c66d0a834b26172d7b2e11cac33`.
- Publication access: br, Homebrew tap and Scoop bucket have Actions disabled; GitHub credentials have push access to both venue repositories. AUR authentication was denied on Mac, ts2 and the controller. For the latter two, verified the host key against the [AUR's published Ed25519 fingerprint](https://aur.archlinux.org/?setlang=en) and used a dedicated known-hosts file. The outstanding user question concerns the publishing identity, not permission to release.
- RCH cache diagnosis: per-command temporary Cargo homes changed registry source paths and forced each shard to rebuild dependencies. Subsequent ovh-a commands will explicitly reuse this campaign's existing Cargo home at `/data/tmp/rch/beads_rust/ce318454fb5948b8/.rch-tmp/rch-cargo-cache-ovh-a`; compiler profiles and assertions remain unchanged. The ts2 cache and target paths are also recorded for a warm retry if its cold 50-target build reaches the declared cap.
- Docker on the Mac completed its existing-container restoration at 2026-09-12 01:42 UTC after a normal application start. A new retained, network-disabled `python:3.12-slim` container executed successfully and reported `aarch64`; no existing containers or processes were stopped or removed. The final seven-target DSR configuration passed `repos validate`. Both are infrastructure checks, not release-binary qualification.
- The 22-target miscellaneous/conformance/snapshot shard passed through RCH at 2026-09-12 01:53 UTC: 3,284 test invocations, zero failures, 72 existing ignores, outer exit zero. This includes the randomized storage model and long sequential dependency-removal regression. The ts2 cold storage build reached its declared 1,800-second cap before tests began (remote/outer 137); its source receipt and cache remain intact. A four-job retry was refused before execution because the dedicated worker advertises two slots. The next invocation preserves two jobs, explicitly reuses the existing Cargo home and target, and declares 3,600 seconds prospectively. No local fallback or test pass is claimed for either refused/timed-out invocation.
- The installed RCH 1.0.64 source-receipt path rewrites the requested target directory underneath a fresh per-build source root; only the explicitly requested Cargo registry cache survived. The bounded ts2 retry is consequently rebuilding dependencies. The ovh-a clean-overlay path uses a durable target pool and now retains the explicit registry cache as intended.
- All-target, all-feature Clippy with `-D warnings` passed through RCH at 2026-09-12 01:57 UTC (3m26s); all-target, all-feature Cargo check passed at 01:58 UTC (1m00s), both outer exit zero. `cargo fmt --check` and `git diff --check` also passed. Default-feature library/binary tests are running next.
- Expanded native canary qualification now passes 95 commands, including actual schema-15 and schema-16 fixtures migrated to schema 19, issue-field/count preservation, unchanged source JSONL, and no-op re-planning. A first invocation used an already-removed RCH source path; retained that failed harness log and reran with separately copied frozen fixtures in a new workspace. Passing development-binary SHA-256: `1ab6de0e785bb2a418e3b0a39936e1f31a8b95612bbdc9b349323fe8b66d0bcf`; receipt `/tmp/br-4e2n1-evidence-20260912/canary-harness-qualification-v4.log`. The eventual release binaries still need the same qualification.
- Version availability checked at 2026-09-12 02:00 UTC: no remote `v0.6.0` tag, GitHub release endpoint 404 and crates.io version endpoint 404. The new prerequisites/workflow capabilities warrant 0.6.0; version changes remain pending completion of the test gate.
- Pinned DSR source review found that its strict ELF validator rejects custom `linux/musl_*` target keys even when configuration validation passes. Corrected the plan before build execution: five canonical primary targets, plus a second two-target recipe using canonical Linux keys mapped to musl triples and explicit musl artifact names. Both recipes validate; packaging retains both original run states and the explicit key mapping. This follows the prior release's successful musl route without altering DSR's validator.
- Default-feature `cargo test --locked --lib --bins` passed through RCH at 2026-09-12 02:05 UTC: 3,058 library tests with two existing ignores, plus 66 binary tests; zero failures and outer exit zero. No-default-feature library qualification is next.
- No-default-feature library tests passed through RCH at 2026-09-12 02:12 UTC: 3,045 passed, zero failed, two existing ignores, outer exit zero. All-feature binary tests and the seven ordinary benchmark test targets are running next; scheduled ignored stress benchmarks remain outside this release test invocation, with no new ignores or filters added.
- All-feature binary tests plus all seven ordinary benchmark targets passed at 2026-09-12 02:13 UTC: 1,259 test invocations, zero failures, 22 existing ignores, outer exit zero. Signing readiness also passed on trj using the unchanged br key `36B847D11BA5A0D0`; its private key stays there. The Mac's unrelated default DSR key `69B3955C8D2E62A8` will not sign br assets. AUR authentication was denied on trj as well.
- `cargo test --locked --all-features --doc` exited zero at 2026-09-12 02:14 UTC, but executed no test cases: all ten existing doctests are ignored. This supplies no additional runtime coverage. The separate `docs_examples` integration target ran in the passed miscellaneous shard; no doctest ignores were added or changed.
- Once ovh-a's other gates finished, moved the unchanged 50-target storage shard to its warm all-feature cache. Gracefully cancelled only owned ts2 build `30017382718111748` at 2026-09-12 02:16 UTC; the cancellation released both slots, the Cargo PID was absent and the isolated queue was empty. No tests ran in that cancelled retry. Retained both ts2 build directories. Fresh ts2 health: load 3.97 on 128 CPUs, 192 GiB available memory and 1.8 TiB disk free. The dedicated release scheduler will advertise eight slots for sequential Linux/Windows cross builds; Mac native builds remain two jobs.
- The full 50-target storage/property/regression/workflow shard passed on ovh-a at 2026-09-12 02:20 UTC: 4,066 test invocations, zero failures, two existing ignores, outer exit zero. The dependency test gate is complete across all-feature library/integration shards, default and no-default library configurations, binary/ordinary benchmark targets, all-target checks and Clippy, formatting and security audit. The doc command executed no cases, as recorded above. Proceeding to 0.6.0 metadata and focused version-sensitive checks; no release binaries or publication yet.

- Release preparation committed as `b1cfebe05437463e91a353cf2bedafac27266f5b`, with local annotated `v0.6.0` tag (not pushed). Focused version-sensitive checks passed through RCH at 2026-09-12 02:25 UTC: 625 tests, zero failures or ignores. The clean Mac release checkout preserves all 4,169 tracked entries. Primary strict DSR build started at 02:30 UTC for five targets; the two musl targets require a separate completed DSR run because the pinned validator accepts canonical Linux platform keys. No release venue is published yet.

- First DSR attempt stopped before compilation because its locked offline source-closure check lacked FastMCP 0.9.0 in the command hosts' ambient registry caches. Fetched the unchanged locked dependencies on Mac and ts2. A retry correctly refused reusing the first output directory; the next attempt uses a fresh output path and retains both refusals. Its two-target concurrency is bounded by one DSR slot per command host, mapping to one RCH build per actual worker; Mac compilation stays at two jobs and ts2 at eight.
- The exact crates.io package contains 1,080 files and has SHA-256 `0d0fbac9a6c83b1ee48ab585f3d5f3fe9c8005a05cab7e039502d9293ed1cecf`. Every extracted file was byte-compared to the archive; a temporary verification-only Git index explicitly includes packaged files ignored by the repository's normal ignore rules. Its default-feature binary compiled through RCH on ovh-a at 2026-09-12 02:39 UTC. Runtime qualification and the optional-feature package build are pending. SPDX and CycloneDX inventories both include all 633 frozen lockfile packages; these are source inventories, not claims that every optional dependency is compiled into each binary.

- Exact extracted-package qualification is complete: default binary passed 95 CLI/migration commands, 29 PTY commands, and all 45 doctor selftest steps; its first doctor invocation failed because the requested parent directory did not exist, then passed after creating that directory. The all-feature package build passed at 02:45 UTC, and all 22 packaged MCP protocol tests passed at 02:52 UTC, with zero failures or ignores. The package SHA remains unchanged. This is focused package qualification, not a claim that fixture-dependent repository tests all run from the published archive.
- DSR run `4659395f-e8d0-4516-8760-d0767213f8cb` now compiles Linux/Windows through RCH. Its two Mac attempts failed before compilation because the dedicated route used Linux's `/data` staging path; corrected only that route's `transfer.remote_base` to `/Users/Shared/dsr-sources/br-4e2n1-rch-mac`. Separate Mac run `5aa83804-8eef-4cbe-939e-f5feb467fff9` is compiling on Apple Silicon. Packaging will explicitly select three successful Linux/Windows results, two successful Mac results, and two successful musl results from their actual run records; no failed target is selected or reclassified.

- First two accepted release binaries passed native qualification: Linux GNU amd64 (`21b967c1ae68df1a2e8eb2256d13b8e57d293d89e331933919076104832ddbc0`, 27,772,512 bytes; max GLIBC 2.28) and Apple Silicon (`822657f6d9d52f4d81e4483467614ca32f96f79dde796c9b2601d05e2cb80401`, 16,138,704 bytes). Each passed 95 CLI/migration commands, 29 real-PTY commands, and 45 doctor selftest steps, reporting the frozen release commit and default `self_update` feature. Windows release compilation reports five unused-code warnings; the retained v0.5.12 Windows log has the same five diagnostics (`windows-warning-baseline-0512.txt`). No new Windows warning is claimed, and the Linux Clippy result is not presented as a Windows Clippy run.

### Execution checklist

- Five of seven raw release binaries are accepted and runtime-qualified. Windows GNU amd64 passed 92 CLI/migration commands and 45 doctor steps; Linux GNU arm64 and Intel macOS each passed 95 CLI/migration commands, 29 PTY commands and 45 doctor steps. Intel execution used Rosetta on Apple Silicon, not physical Intel hardware. GNU arm64's maximum required GLIBC is 2.28.
- The first Intel DSR attempt returned 137 without a valid result, although its underlying RCH job subsequently returned zero. That artifact was not selected. The resumed DSR attempt succeeded. DSR reused its compiler-log pathname; the attempted archive raced with truncation and contains early retry output. The original result and orchestration logs remain, but the original full compiler log does not. `darwin-amd64-log-retention-note.txt` documents this limitation.
- The Linux arm64 migration canary failed on a writable macOS Docker bind mount with a database-identity change. The published 0.5.12 binary reproduces the same failure; the 0.6.0 binary passes the entire canary on a native Docker volume. Open P1 bead `beads_rust-q93wv` tracks the limitation. Retained both failed workspaces; no migration repair is claimed. Release notes identify native macOS or native Linux storage as the qualified migration routes.
- Musl amd64 compilation returned zero at 2026-09-12 03:39 UTC; DSR artifact collection remains pending. Musl arm64 was refused before compilation by RCH's active-project exclusion, despite available slots. Its original refusal/result were archived before resuming; subsequent builds will run sequentially without bypassing that exclusion.
- The exact crates.io publication dry run passed and its actual upload payload (`package/tmp-crate/beads_rust-0.6.0.crate`) matches the qualified SHA-256. Nothing has been published. Both DSR-verified SBOMs cover all 633 locked packages.

- [x] Read project and skill instructions; check clean tracked source and previous release records.
- [x] Inventory every direct dependency against the registry and lockfile.
- [x] Claim release bead and reserve manifest, lockfile, and upgrade logs.
- [x] Complete stable-tag API and dependency-pin research.
- [x] Finish baseline tests; update the coupled dependency unit and pass its library tests.
- [x] Record retained exact pins and audit the resolved dependency graph.
- [x] Pass all-feature library and targeted MCP/package-manifest gates.
- [x] Pass all 38 `e2e-a-l` targets through RCH.
- [x] Pass all 39 `e2e-m-z` targets through RCH.
- [x] Pass all 50 storage/property/regression/workflow targets through RCH.
- [x] Pass miscellaneous/conformance/snapshot targets using pinned bd 0.46.0 and bv 0.22.0.
- [x] Pass default-feature tests and the no-default-feature library gate.
- [x] Pass binary tests and ordinary non-ignored benchmark targets; run the doc gate and explicitly record its zero executable cases.
- [x] Pass all-target Cargo check and Clippy with warnings denied; verify formatting.
- [x] Qualify the expanded native release canary against the Linux development binary.
- [x] Update changelog from verified commits; select and bump the next available release version after the test gate.
- [x] Freeze source, version, features and lockfile.
- [x] Build Linux GNU amd64/arm64, Linux musl amd64/arm64, macOS amd64/arm64, and Windows amd64 through RCH using the established DSR release flow. The resumed musl run completed 2/2 with post-build source validation at 2026-09-12 03:56 UTC.
- [x] Verify canonical archive contents, seven-target size budgets, SHA sidecars, Minisign signatures and SBOMs.
- [x] Run CLI/migration canaries, Unix PTY checks and doctor selftests on all seven release binaries; verify GNU ABI floors, static musl linkage and Windows DLL imports. Intel macOS execution used Rosetta; all other target execution was native.
- [x] Stage the GitHub draft, download/verify all 24 assets and exercise the downloaded binaries before publication.
- [x] Review qualification claims before publication: source tests and raw-binary gates passed without new ignores or weaker assertions; documented existing doctest ignores, Windows warnings and bind-mount migration failure. Infrastructure refusals/timeouts are not passes. Intel execution is Rosetta, and its first full compiler log was not retained. These are operator-run checks, not independent certification.
- [x] Publish GitHub and verify all 24 assets again through unauthenticated public downloads.
- [x] Package the frozen crate and byte-compare all 1,080 extracted files.
- [x] Compile the exact package's default and all-feature binaries through RCH; pass CLI, migration, PTY, doctor, and MCP protocol checks.
- [x] Publish that exact package and verify the crates.io registry checksum.
- [x] Update and verify the Homebrew tap and real installation.
- [x] Update and verify the Scoop bucket and real Windows installation.
- [x] Build both Arch binary packages and verify native amd64 installation/integrity before AUR publication.
- [ ] Publish and read back AUR when an authorized SSH identity is available; retain the explicit blocker meanwhile.
- [x] Exercise the public installer and an actual old-to-new self-update.
- [x] Commit publication metadata and evidence summaries; close only fulfilled obligations. Historical AUR blockers stay separate.

---

**Date:** 2026-09-04 | **Project:** beads_rust | **Language:** Rust (nightly-2026-08-31, edition 2024) | **Manifest:** Cargo.toml

## Summary (2026-09-04)

- **Inventory:** 56 direct dependency entries (41 `[dependencies]`, 1 build, 14 dev); 20 entries behind crates.io's latest stable at start: the `fsqlite*` family (15 crates, 0.3.15 → 0.3.16, published 2026-09-04), `asupersync` (=0.4.9 → 0.4.10, two entries), `fastmcp-rust` (=0.7.1 → 0.8.1), `toon_rust`/`tru` (0.2.3 → 0.2.4), `toml` (dev, =1.1.4 → 1.1.5). Everything else already at latest stable (vergen-gix 10.0.3, rand 0.10.2, clap 4.6.6, serde 1.0.229, chrono 0.4.45, regex 1.13.1, thiserror 2.0.20, insta 1.48.0, proptest 1.11.0, criterion 0.8.2, ...).
- **Method:** one dependency at a time; research from crates.io metadata and the upstream changelog/compare; manifest + lockfile update; the relevant test targets through RCH (`rch exec -- cargo ...`; RCH caps one command at 5 min for builds and 30 min for tests, so each entry names the exact targets run); log here before moving on.
- **Order:** tru → toml → fastmcp-rust → fsqlite family (engine-bump checklist, `docs/reliability/ENGINE_OPERATING_MODEL.md` §6). asupersync stays at =0.4.9: fastmcp-rust 0.8.1 still pins `=0.4.9` exactly and the `mcp` build must carry one asupersync (bead beads_rust-fiop); fsqlite 0.3.16 accepts `>=0.4.3,<0.5`.
- **Result:** updated 3 lines (fsqlite family ×15 manifest entries / 20 crates, fastmcp-rust ×8 crates plus its `log` pin, tru), skipped 2 (asupersync, toml — both held by fastmcp's exact pins), failed 0, rolled back 0. Landed as one commit (5e81e796) after every dependency had passed its own RCH gates; the hosted CI push run and a dispatched `Reliability Gates` run are the receipts for the whole tree (ids below).

## Updates (2026-09-04)

### toon_rust (`tru`): 0.2.3 → 0.2.4

- **Changelog:** [v0.2.3...v0.2.4](https://github.com/Dicklesworthstone/toon_rust/compare/v0.2.3...v0.2.4): installer download retries, hardened git-metadata detection in its build.rs, dependency bumps (clap_complete, assert_cmd, vergen-gix 9→10), and its *optional* asupersync pinned to `=0.3.4` — a feature br does not enable, so no second asupersync enters the graph (verified against the crates.io dependency list for 0.2.4). No changes to `encode`/`encode_lines`/`EncodeOptions`/`KeyFoldingMode`, the surface br uses in `src/output/context.rs`.
- **Breaking changes:** none for br's usage.
- **Lockfile:** `cargo update -p tru` moved only `tru`; the same write pruned sixty orphaned entries (the old vergen 9 / `gix-*` tree that nothing referenced since vergen-gix moved to 10) — no other version changed, vergen-gix stays 10.0.3.
- **Tests (RCH):** `cargo clippy --lib --bins -- -D warnings` clean; `cargo test --lib output::` 41/41; `cargo test --test e2e_create_output` (TOON output) 7/7.

### fastmcp-rust (optional `mcp` feature): =0.7.1 → =0.8.1

- **Changelog:** [v0.8.1](https://github.com/Dicklesworthstone/fastmcp_rust/releases/tag/v0.8.1) (first published 0.8; v0.8.0 was a quarantined candidate). Pre-1.0 minor release: caller-owned asupersync contexts at library boundaries (client constructors take `&Cx`; returning server runners and custom-transport runners require a caller-owned context), the facade no longer exports `block_on`, cancel-correct admission/cleanup, transport fixes (stdio partial-frame deadline, WebSocket cancellation, SSE write-half close), packaging (redis-tasks and safe-icon-rendering features removed; plist/quick-xml advisory clear; `license-file` metadata).
- **Breaking changes for br:** none. `br serve` already builds its own current-thread asupersync runtime, mints a request `Cx`, and runs `ServerBuilder::…build().run_transport_returning_with_cx(&cx, StdioTransport::stdio())` (`src/mcp/mod.rs`), which is exactly the caller-owned-context shape 0.8 requires. No source changes.
- **Pins:** fastmcp-rust/fastmcp-client 0.8.1 keep the exact pins `asupersync =0.4.9`, `serde =1.0.229`, `serde_json =1.0.151`, `toml =1.1.4`, `rustix =1.1.4` (all equal to what br already resolves) and add `log =0.4.34`, which moved `log` 0.4.33 → 0.4.34 in the lockfile. The eight `fastmcp-*` workspace crates moved together (0.7.1 → 0.8.1); nothing else changed. The manifest comment on the asupersync pin now cites 0.8.1.
- **Tests (RCH):** `cargo test --lib --features mcp mcp::` 75 passed / 7 ignored (pre-existing ignores); `cargo test --features mcp --test e2e_mcp_protocol` 1/1; `cargo test --features mcp --test e2e_mcp_shutdown` 1/1 (SIGINT returns through `main` and the DB reopens — the runtime-ownership change did not disturb the cancellation path); `cargo clippy --lib --bins --features mcp -- -D warnings` clean.

### fsqlite family (15 manifest entries, 20 crates in the lock): 0.3.15 → 0.3.16

- **Changelog:** [v0.3.16](https://github.com/Dicklesworthstone/frankensqlite/releases/tag/v0.3.16) (2026-09-03; crates.io 2026-09-04). Engine-relevant items per the §6 checklist (pager/WAL/B-tree/checkpoint/VFS):
  - **Pager:** the EOF-growth double-grant is closed (bd-9inpb, `6f61702f9`). Two connections growing the file concurrently could both allocate the same fresh EOF page and commit it ("2nd reference to page N", lost rows); `commit_flush` now re-derives the pre-floor snapshot size under the RESERVED append lock and refuses a batch whose fresh pages fall in `(snapshot_db_size, durable_floor]` with a retryable `BusySnapshot` (first committer wins). Upstream repro: 3 double-grants in 76 eight-writer runs before, 0 in 80 after; perf-neutral at 1–8 writers. This is the corruption class br's concurrent writers live next to, so it is the reason to take the bump.
  - **WAL / checkpoint / open:** `reclaim_disowned_in_range` (run by `checkpoint` and by the on-open reclamation sweep) no longer rescans every WAL frame header per page; an `AppendedTailIndex` keyed on generation, frame count, and last-frame checksum indexes a stable tail once (`8d012706a`, cass GH#382). Same answers, bounded cost on large WALs.
  - **Not relevant to br:** the FTS5 lazy-read, savepoint undo-log, and incremental-append work (br creates no FTS5 tables); the upstream lockfile refresh (their manifest ranges are unchanged; asupersync stays `>=0.4.3,<0.5`, satisfied by our `=0.4.9`).
  - **Open escalation:** frankensqlite#407 (bead ro3m) was fixed upstream on 2026-09-04 in `007822add`/`efdf9e2a0`, eleven and fourteen commits **after** the v0.3.16 tag. The probe `grouped_having_in_subquery_count_with_bound_params --ignored` still fails on 0.3.16 (baseline on 0.3.15 also failed), so the `multi_label_and` counting detour and the ignore stay; the ignore text, the code comment, and the §7 row now say 0.3.16 and point at the fix.
- **Breaking changes:** none; no API change in the facade br uses (`src/franken_sync.rs`). `tinyvec` stays 1.12.0 in our lock (upstream notes 1.13.0 fails to compile for them).
- **Lockfile:** the 15 manifest crates plus the five `fsqlite-ext-*` crates moved 0.3.15 → 0.3.16; no other version changed.
- **Tests (RCH, §6 items 2, 5, 6):** `cargo test --lib` 2969 passed / 4 ignored (150 s on hz4); `cargo test --test model_based_storage` 163/163 (the 120-case property run plus the GH#426 chain and the blocker-direction regression; 864 s on a worker shared with two other cold builds); `cargo test --test linearizability_multiprocess -- --nocapture` 166/166 — 361 operations in 30 s over eight process streams, none failed, every history linearizable, 30 issues observed at quiescence, published JSONL equal to the observed final state; `cargo clippy --lib --bins -- -D warnings` clean; the ro3m probe re-run as above.
- **Stress gate and doctor (§6 items 3–4):** not runnable locally (RCH cannot deliver a built `br` within its caps); the hosted CI `Reliability Gates` job (manual dispatch) runs the failure-corpus replay, crash-injection matrix, the single-workspace and concurrent stress harnesses, the multi-process stress, and the linearizability check on the pushed tree — its run id is recorded below once it completes.

## Skipped (2026-09-04)

- `toml =1.1.4` (dev; 1.1.5 available): `cargo update -p toml` refuses — `fastmcp-client` pins `toml = "=1.1.4"` exactly, at 0.7.1 and still at 0.8.1, and the `mcp` build can carry only one `toml` 1.1.x. 1.1.5 is a single fix (`DeValue::make_owned` on integers/floats) that br's manifest tests do not exercise. Revisit when fastmcp-client moves its pin.
- `asupersync =0.4.9` (0.4.10 available): held by fastmcp-rust 0.8.1's exact `=0.4.9` pin; 0.4.10 is observability/regex scanner work (bounded PII, payment-card, and phone scanners) with no runtime-contract change noted, so nothing is lost by waiting for fastmcp to move its pin.

---

**Date:** 2026-08-14 | **Project:** beads_rust | **Language:** Rust

## Summary

- **Updated:** fsqlite family (15 crates) 0.1.18 → 0.3.1; new direct `asupersync =0.4.4`; FastMCP's asupersync line 0.3.9 → 0.3.10; 11 minor/patch lockfile bumps | **Skipped:** 2 (with reasons) | **Failed:** 0

## Discovery

- Manifest: `Cargo.toml`; lock file: `Cargo.lock`.
- crates.io max stable at completion: `fsqlite* = 0.3.1` (all 15 pinned members published), `asupersync = 0.4.4`, `fastmcp-rust = 0.3.2` (unchanged; still on the asupersync 0.3.x line).
- All other direct dependencies were already at latest stable or covered by existing caret ranges; only lockfile refreshes were needed (supersedes Dependabot PR #425).

## Updates

### fsqlite stack: 0.1.18/0.1.19 → 0.3.1 (with asupersync 0.4.4)

- **Breaking (upstream 0.2.0):** the entire engine API became `async fn` with `!Send` futures (`Connection::open`, `execute*`, `query*`, `prepare`, `close*`, `compat::open_with_flags`).
- **Breaking (upstream 0.3.0):** the runtime family moved from asupersync 0.3.10 to `>=0.4.3,<0.5`; 0.3.x and 0.4.x asupersync types are non-interchangeable.
- **Migration:** added `src/franken_sync.rs`, a synchronous facade that drives every engine future to completion on the calling thread via a thread-local current-thread `asupersync` Runtime (`Runtime::block_on`; the proven cass/sqlmodel bridge pattern). The runtime is taken out of its slot while polling so reentrant SQL builds a fresh runtime instead of re-entering `block_on`. The facade carries a bounded `BusyRecovery` retry (restores 0.1.x observable behavior around fsqlite 0.2+ ns-lifecycle recovery windows) and a stale-schema `prepare()`-refresh retry (fsqlite 0.2.1+ cross-connection DDL visibility). All `Connection`/`Row` imports across storage, sync, config, doctor subsystems, CLI, and integration tests moved to `crate::franken_sync::` / `beads_rust::franken_sync::`; `Row`, `SqliteValue`, and `FrankenError` re-export unchanged. Every writable open, including the explicit read-write compatibility path used by reconciliation, selects serialized engine mode to match br's workspace write lock. Missing-database recovery now quarantines all orphaned fsqlite 0.3 sidecars into verified backups before rebuilding from JSONL. `Drop` drives a best-effort close so writes through a dropped connection stay visible to later opens (#270 contract).
- **asupersync:** new direct dependency `asupersync = { version = "=0.4.4", default-features = false }` (initially =0.4.3; bumped same day when upstream published 0.4.4), matching the fsqlite family requirement so one runtime version serves the whole default graph. The 0.4.4 cancellation-contract refinement (spawned-task results surviving cancel acknowledgement) does not affect br's `block_on` bridge, which spawns no tasks.
- **mcp feature caveat:** published `fastmcp-rust 0.3.2` still requires `asupersync ^0.3.4`, so `--features mcp` builds carry both asupersync 0.3.x and 0.4.4 (they are distinct crates under Cargo's 0.x rules and coexist). This resolves to a single 0.4.4 line once fastmcp republishes against 0.4.x.
- **Engine-fix relevance:** fsqlite 0.3.0/0.3.1 fix the allocator page-aliasing, committed-freelist resurrection, and concurrent-writer EOF-growth corruption classes plus concurrent-open `BusyRecovery` fail-fasts — the classes behind beads_rust issues #426 and #428 and the concurrent-open regression that blocked the earlier (abandoned) `harmonize/vlsf2` migration attempt.
- **Tests:** see Validation below.

### Minor/patch dependency updates (supersedes Dependabot PR #425)

- clap 4.6.4 → 4.6.6, clap_complete 4.6.7 → 4.6.9, schemars 1.2.1 → 1.2.2, similar 3.1.1 → 3.1.2 (manifest floors + lock).
- toml (dev-dependency, exact pin) =1.1.2 → =1.1.4.
- FastMCP's independent asupersync line 0.3.9 → 0.3.10, including its
  `franken-{kernel,evidence,decision}` 0.3.10 family and consolidated crypto
  dependency graph.
- lru 0.18.1 → 0.18.2 for fsqlite-core/fsqlite-planner, fixing
  RUSTSEC-2026-0253's panic-safety use-after-free in `LruCache::pop`.
- Lockfile-only refreshes: thiserror 2.0.20, libc 0.2.189, once_cell 1.21.4, regex 1.13.1, flate2 1.1.9.
- **Breaking:** none found for this project's usage in any of these lines.

### Lint-gate remediation (issue #409 cluster E)

- The 2026-08 nightly clippy added `assert_is_empty` (pedantic), which fired ~125 times on test `assert!(x.is_empty())` calls; added to the Cargo.toml stylistic allow-list alongside the existing entries (rewriting those asserts is churn, not safety).
- The remaining ~100 pedantic/nursery findings in the merged doctor/sync workstream code were fixed individually (renamed used-underscore bindings, by-ref parameters, heap-allocating the 1 MiB and 64 KiB stack buffers, boxing the large `PendingSyncMergeInspection::Valid` variant, `let...else` rewrites, merged match arms, `trailing_zeros` bit tests, per-function `too_many_lines` allows per codebase pattern, and documented targeted allows where a fix would change cross-file signatures or MSRV-unavailable APIs are involved).

## Skipped

- `self_update 1.0.0-rc.x`: pre-release line retained (crates.io max stable is the older 0.44); per policy, pre-release pins are preserved.
- `cap-primitives = "=4.0.2"`: exact pin retained by design (sync's hostile-path boundary).

## Needs Attention

- `fastmcp-rust`: republish against asupersync 0.4.x will let the `mcp` feature collapse to a single asupersync (tracked informally; sibling checkout already pins =0.4.3 at version 0.3.2, unpublished).
- `rich_rust 0.2.2` retains lru 0.16.4, which cargo-audit reports under the
  same informational panic-safety advisory. Its caches use ordinary
  `String`/`Style` keys rather than caller-provided panicking `Drop` types;
  upgrading requires a new `rich_rust` release because 0.2.2 constrains lru
  to the 0.16 line.

## Validation

- `cargo check --all-targets` passed after the migration.
- `cargo fmt --check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  (pedantic + nursery at deny).
- `br serve` SIGINT shutdown test passes
  (`e2e_mcp_shutdown::serve_sigint_returns_through_main_and_preserves_reopenable_db`)
  after fixing a same-process write-lock self-deadlock that predated the
  engine upgrade.
- Targeted regression suites on the settled tree: `e2e_read_only_fast_open`
  160/160, `e2e_sync_reconcile` 180/180, `e2e_sync_failure_injection`
  179/179, `e2e_sync_status_health` 166/166, `e2e_sync_artifacts` 169/169,
  doctor fixture suite 65/65, storage_deps + e2e_relations cycle clusters
  green.
- Full `cargo test --all-features --no-fail-fast` on the settled tree:
  **21,490 passed, 0 failed** across every test binary (doctests included),
  up from 21,415 passed / 70 failed at the start of the migration wave.
