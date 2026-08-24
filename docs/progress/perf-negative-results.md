# Performance Negative Results

This ledger records performance ideas that were measured and rejected. Check it before starting a new optimization pass, and add an entry whenever a candidate is abandoned, reverted, or kept out of the tree because the benchmark matrix did not move in the intended direction.

Entries preserve failed experiments as reusable evidence. A retry is justified only when its stated predicate is satisfied by a current profile or architectural change. Historical measurements remain tied to their recorded hosts, binaries, fixtures, and artifacts; they are not current performance claims.

## Preflight coverage

### 2026-08-23 — CASS preflight coverage — blocker

- **Hypothesis:** The last 60 days of agent sessions may contain rejected `beads_rust` optimization attempts that are absent from repository artifacts.
- **Workload(s) probed:** Exact workspace-path and `beads_rust` queries combined with `rejected`, `reverted`, `abandoned`, `slower`, `regressed`, `within noise`, `no improvement`, and `not a keep` across local, `css`, `csd`, `ts1`, and `ts2` CASS indexes.
- **Measurement summary:** The local lexical index was about 5.5 days stale and exact local workspace-path queries returned no useful attempt. The broad cross-machine query returned 11 hits on `css`, 51 on `csd`, 22 on `ts1`, and 8 on `ts2`; context spot-checks showed prompt/attachment co-occurrence and unrelated projects rather than a reliable additional `beads_rust` rejection. Repository artifacts and recent Git history were therefore used as the durable evidence source.
- **Outcome:** blocker
- **Scratch worktree:** not applicable; read-only evidence mine
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-swarm-program-closeout/summary.md`
- **Retry-condition predicate:** Blocked until the local CASS lexical index is refreshed and exact workspace-scoped queries return context-bearing results; track as `beads_rust-7kw0`.
- **Bead id (if applicable):** `beads_rust-7kw0`
- **Commit (if attempted):** uncommitted

## Measured rejections

### 2026-05-04 — Parallel JSONL import preparation — reverted

- **Hypothesis:** Bounded parallel JSONL preparation above the storage layer would reduce fresh forced-import time for a 12,000-record corpus.
- **Workload(s) probed:** Focused serial-versus-parallel normalization/order checks and the broad 12,000-record fresh `sync --import-only --force --json` workload.
- **Measurement summary:** Behavior checks passed, but wall time regressed from `4:36.17` to `4:46.67`, user CPU from `275.57s` to `286.12s`, and peak RSS from `145540 KB` to `171208 KB`. The source diff was reverted.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-jsonl-import-descope-current-20260504-EBgRQt`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-jsonl-import-descope/summary.md`
- **Retry-condition predicate:** Reconsider only inside the broader `fsqlite` prepared or bulk-DML redesign.
- **Bead id (if applicable):** `beads_rust-72yf.5.1`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Scheduler eager materialization deferral — rejected

- **Hypothesis:** Deferring `ReadyIssue` conversion and rationale construction until after score, sort, and truncate would reduce scheduler CPU and allocation cost.
- **Workload(s) probed:** `scheduler --json --candidate-limit 512 --limit 20`, with normalized JSON equality.
- **Measurement summary:** Normalized SHA-256 matched. Baseline was `893.2 ms +/- 18.6 ms`; candidate was `903.6 ms +/- 13.1 ms`, with the baseline `1.01 +/- 0.03` times faster.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-candidate-scheduler-materialization-20260504`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-scheduler-materialization/summary.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 10% to `ReadyIssue` conversion and rationale construction on `scheduler --candidate-limit 512 --limit 20` or a wider scheduler workload.
- **Bead id (if applicable):** `beads_rust-72yf.19`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Scheduler narrow candidate hydration — within-noise

- **Hypothesis:** Scoring a narrow projection and rehydrating only selected scheduler rows would avoid unnecessary candidate hydration.
- **Workload(s) probed:** `scheduler --json --candidate-limit 512 --limit 20`, with normalized JSON equality.
- **Measurement summary:** Baseline was `222.2 ms +/- 9.5 ms`; candidate was `219.5 ms +/- 9.7 ms`, a reported `1.01x +/- 0.06x` difference and therefore noise. The probe was reverted.
- **Outcome:** within-noise
- **Scratch worktree:** `/data/tmp/br-candidate-scheduler-narrow-20260504-local`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-scheduler-narrow-hydration/summary.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 10% to full candidate-row hydration on a scheduler workload with at least 512 candidates.
- **Bead id (if applicable):** `beads_rust-72yf.20`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Scheduler deferred rationale allocation — rejected

- **Hypothesis:** Constructing scheduler rationale strings only after result truncation would reduce allocation time.
- **Workload(s) probed:** `scheduler --json --candidate-limit 512 --limit 20`, with normalized JSON equality.
- **Measurement summary:** Baseline was `221.7 ms +/- 7.8 ms`; candidate was `225.2 ms +/- 6.8 ms`, with the baseline `1.02x +/- 0.05x` faster. The probe was reverted.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-candidate-scheduler-rationale-defer-20260504-local`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-scheduler-narrow-hydration/summary.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.20`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — TOON stats-off length-pass removal — rejected

- **Hypothesis:** Skipping encoded-line length calculation when TOON statistics are disabled would reduce unlimited TOON list latency.
- **Workload(s) probed:** `list --limit 0 --format toon`, with byte-for-byte output equality.
- **Measurement summary:** Output SHA-256 matched. Baseline was `312.9 ms +/- 10.7 ms`; candidate was `317.6 ms +/- 8.9 ms`, with the baseline `1.02 +/- 0.04` times faster. The probe was reverted.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-candidate-toon-length-pass-20260504`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-toon-length-pass/summary.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 5% to TOON encoded-line length calculation on an unlimited structured-output workload.
- **Bead id (if applicable):** `beads_rust-72yf.21`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — TOON iterator list streaming — rejected

- **Hypothesis:** Avoiding the final `Vec<IssueWithCounts>` materialization with an iterator writer would reduce unlimited TOON list latency.
- **Workload(s) probed:** `list --limit 0 --format toon` on a 12,000-issue matrix, with byte-for-byte output equality.
- **Measurement summary:** Baseline was `212.8 ms +/- 6.9 ms`; candidate was `214.1 ms +/- 4.7 ms`, with the baseline `1.01x +/- 0.04x` faster. The probe was reverted.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-candidate-list-toon-iter-20260504-local`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-list-toon-iterator-stream/summary.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 5% to final list materialization on an unlimited TOON workload.
- **Bead id (if applicable):** `beads_rust-72yf.22`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Single-label full page hydration — rejected

- **Hypothesis:** Hydrating all materialized label candidates in Rust, filtering and sorting them, then truncating would beat the SQL page path for one label.
- **Workload(s) probed:** `list --limit 50 --json --label export` and the corresponding plain-text command on a high-cardinality label.
- **Measurement summary:** JSON regressed from `3.41s` to `6.19s`; the candidate plain-text path measured `6.13s`. The probe was removed.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-target-list-query-release`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-single-label-page-hydration/README.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.23`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Single-label top-K lean projection — rejected

- **Hypothesis:** Loading only `id`, status, priority, creation time, and template state for label candidates, selecting top K, then hydrating those rows would avoid the full-hydration loss.
- **Workload(s) probed:** `list --limit 50 --json --label export` on a high-cardinality label, with the narrow `lane-00` shape observed but not fully rerun.
- **Measurement summary:** The broad-label command regressed from `3.43s` to `6.46s`; the narrow-label candidate measured `0.28s` but was inconclusive. The probe was removed.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-target-list-query-release`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-single-label-topk-projection/README.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.24`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Redundant broad-label preflight — reverted

- **Hypothesis:** Proving a broad label redundant before candidate materialization would speed broad label queries without harming narrow labels.
- **Workload(s) probed:** Broad `export` and narrow `lane-00` label shapes for both list and search.
- **Measurement summary:** Broad list/search improved only `1.02x` and `1.04x`; narrow list regressed from `243.5 ms` to `256.8 ms`, and narrow search from `214.1 ms` to `235.9 ms`. No source change was retained.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-redundant-label-preflight/final/README.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 10% to label-candidate materialization on both broad and narrow label workloads.
- **Bead id (if applicable):** `beads_rust-72yf.27`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Stats label grouped SQL path — reverted

- **Hypothesis:** Reusing the broad filtered label-count query for `stats --by-label` would outperform an in-memory scan of already-loaded issue rows and unordered label pairs.
- **Workload(s) probed:** `stats --by-label --json`, with stable `--no-activity` output equality.
- **Measurement summary:** The grouped SQL candidate regressed from `201.7 ms +/- 5.4 ms` to `338.4 ms +/- 10.3 ms`. It was replaced by the retained unordered-pair scan, which measured `175.7 ms +/- 2.7 ms` against a `204.7 ms +/- 3.6 ms` control.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-stats-label-breakdown/final/README.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.28`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Default label-count SQL specializations — rejected

- **Hypothesis:** A direct label grouping plus no-label anti-join, or a left-join grouping plus scalar visible total, would speed default `count --by label --json`.
- **Workload(s) probed:** Default-visible label count over 12,000 issues and 36,000 label rows, with output SHA-256 equality.
- **Measurement summary:** The better rejected variant regressed from `287.7 ms +/- 7.1 ms` to `323.5 ms +/- 8.9 ms`; the accepted prior binary was `1.12x` faster.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-count-label-default/summary.md`
- **Retry-condition predicate:** Reconsider only inside the broader label-count storage-query redesign.
- **Bead id (if applicable):** `beads_rust-72yf.16`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Search all-priority bucket probing — reverted

- **Hypothesis:** Scanning every priority bucket separately would speed broad default first-page search.
- **Workload(s) probed:** Broad `search payload --json` and sparse `search zzz-no-match --json`, with byte-identical outputs.
- **Measurement summary:** The broad path improved, but the no-match guard regressed from `65.8 ms` to `105.6 ms`. It was replaced by the retained critical-bucket-plus-tail design, whose no-match path measured `80.2 ms` against `65.2 ms` while keeping the target broad search below `143 ms`.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-search-default-page/final/README.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.29`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Broad default list critical-bucket tail — within-noise

- **Hypothesis:** Replacing the priority-bucket loop with one critical-priority query followed by one ordered tail query would speed broad default list pages.
- **Workload(s) probed:** JSON and TOON `list --limit 50`, plus JSON `list --limit 1`, with a focused ordering proof.
- **Measurement summary:** JSON limit 50 moved from `146.2 ms +/- 3.9 ms` to `146.7 ms +/- 4.1 ms`; TOON limit 50 from `150.4 ms +/- 4.0 ms` to `149.9 ms +/- 2.0 ms`; JSON limit 1 from `72.3 ms +/- 0.4 ms` to `72.5 ms +/- 1.1 ms`. The probe was reverted.
- **Outcome:** within-noise
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-list-default-tail/final/README.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 10% to repeated priority-bucket probes on a broad first-page list workload.
- **Bead id (if applicable):** `beads_rust-72yf.31`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Ready cold-column-only projection — rejected

- **Hypothesis:** Removing cold columns from the limited ready-query projection would move `ready` latency.
- **Workload(s) probed:** `ready --limit 20 --format text` and unlimited text output, with output parity.
- **Measurement summary:** Limited text regressed from `3.107s` to `3.210s`; unlimited text was flat at `3.239s` versus `3.229s`. The subsequent retained hybrid blocked-cache filter showed the true lever, improving limited text from `3.137s` to `0.300s`.
- **Outcome:** rejected
- **Scratch worktree:** `/tmp/br-blocked-projection-real2-tTf4Nx`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260503T-ready-window-hydration/summary.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.18`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Text-list full expression index — within-noise

- **Hypothesis:** An index on `COALESCE(priority, 2), created_at DESC, id` would accelerate bounded text list ordering.
- **Workload(s) probed:** `list --limit 20 --format text`, with byte-identical output.
- **Measurement summary:** Baseline was `173.0 ms +/- 5.1 ms`; the expression-index candidate was `173.5 ms +/- 3.8 ms`, a flat result. The retained priority-window query instead measured `144.7 ms +/- 2.4 ms` against `174.3 ms +/- 6.9 ms`.
- **Outcome:** within-noise
- **Scratch worktree:** `/tmp/br-blocked-projection-real2-tTf4Nx`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-list-text-priority-window/summary.md`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 10% to default-order sorting on a bounded text-list workload.
- **Bead id (if applicable):** `beads_rust-72yf.30`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Large structured-list threshold at 64 — reverted

- **Hypothesis:** Lowering the full-scan threshold from 128 to 64 would extend the large-page win to medium structured pages.
- **Workload(s) probed:** JSON list pages at limits 64, 95, 96, 100, and unlimited, with byte-identical outputs.
- **Measurement summary:** The initial threshold regressed the exact limit-64 boundary from `172.2 ms` to `185.6 ms`. The retained threshold of 96 preserved the lower range and improved limit 96 from `219.6 ms` to `180.8 ms` and limit 100 from `223.4 ms` to `182.3 ms`.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/br-list-json-medium-page-20260504-2036/notes.md`
- **Retry-condition predicate:** Worth reconsidering when the full-scan crossover moves below a measured page limit of 64 on the same corpus shape.
- **Bead id (if applicable):** `beads_rust-72yf.34`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Multi-query stats aggregation — rejected

- **Hypothesis:** Several small aggregate SQL queries would outperform one narrow issue scan for `stats` summary output.
- **Workload(s) probed:** `stats --json` and `stats --no-activity --json`, with byte-identical outputs.
- **Measurement summary:** The aggregate multi-query candidate was observably slower; the artifact records the causal finding that several small `fsqlite` queries cost more than one narrow scan. The retained one-scan projection improved the focused no-activity rerun from `131.1 ms` to `124.7 ms`.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/br-stats-summary-20260504-1956/notes.md`
- **Retry-condition predicate:** Reconsider only inside the broader `fsqlite` multi-query dispatch or prepared-statement redesign.
- **Bead id (if applicable):** `beads_rust-72yf.33`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Fresh-import label batching — reverted

- **Hypothesis:** Batching label relation inserts would reduce per-row SQL execution overhead during a fresh JSONL import.
- **Workload(s) probed:** The 12,000-record fresh forced-import corpus, with JSONL hash, dirty-state, doctor, and label-hash correctness witnesses.
- **Measurement summary:** Baseline was `3:01.07` wall, `179.94s` user, and `171676 KB` RSS; candidate was `3:28.14` wall, `206.88s` user, and `186592 KB` RSS. Correctness witnesses matched and the source diff was reverted.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-fresh-label-batch-candidate-20260504-eNtLJA`
- **Profile evidence:** `.beads/issues.jsonl` comment `566` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Reconsider only inside the broader `fsqlite` VDBE or storage bulk-DML redesign.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted; evidence persisted by `a831dfacdb09d87888f5c9d388f0db6c926adc7d`

### 2026-05-04 — Prepared DML for label inserts — reverted

- **Hypothesis:** Reusing prepared DML for label relations would reduce statement preparation and dispatch cost during fresh import.
- **Workload(s) probed:** The 12,000-record fresh forced-import corpus, with JSONL hash, dirty-state, doctor, and label-hash correctness witnesses.
- **Measurement summary:** Baseline was `3:01.07` wall, `179.94s` user, and `171676 KB` RSS; candidate was `3:04.71` wall, `183.57s` user, and `180248 KB` RSS. Correctness witnesses matched and the source diff was reverted.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-prepared-label-candidate-20260504-5y3ZTC`
- **Profile evidence:** `.beads/issues.jsonl` comment `571` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Reconsider only inside the broader `fsqlite` VDBE or storage bulk-DML redesign.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Batched import comment relations — abandoned

- **Hypothesis:** Batched comment-relation writes would reduce the cost of relation-rich JSONL import.
- **Workload(s) probed:** The duplicate-ID, relation-rich 12,000-record import corpus with output and relation-order checks.
- **Measurement summary:** Candidate `4:52.87` wall and `188028 KB` RSS looked faster than its paired `5:04.32` control but was flat against the committed `4:53.11` best, increased RSS from `173784 KB`, and exposed a duplicate-issue-ID correctness trap. The source was reverted.
- **Outcome:** correctness-abandoned
- **Scratch worktree:** historical uncommitted candidate recorded on `beads_rust-72yf.5`
- **Profile evidence:** `.beads/issues.jsonl` comment `552` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Do not retry from a cold read; use comprehensive-bench attribution instead.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Fresh-import projection rebuild skip — rejected

- **Hypothesis:** Skipping blocked-cache and child-counter rebuilds for a fresh import would remove material post-import work.
- **Workload(s) probed:** The 12,000-record fresh forced-import corpus with JSONL, status, doctor, and label-count checks.
- **Measurement summary:** Corrected candidate measured `3:06.87` versus a `3:01.07` baseline and still reported the rebuild. An unsafe upper-bound probe that forcibly skipped both rebuilds reached only `3:00.01`, within noise. All probe code was reverted.
- **Outcome:** rejected
- **Scratch worktree:** `/data/tmp/br-candidate-projection-upper-20260504`
- **Profile evidence:** `.beads/issues.jsonl` comments `564` and `569` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Import content-hash reuse — within-noise

- **Hypothesis:** Reusing computed issue content hashes during import would remove meaningful repeated hashing.
- **Workload(s) probed:** The 12,000-record fresh forced-import corpus with JSONL, status, doctor, and label-hash checks.
- **Measurement summary:** Baseline was `3:01.07` wall and `171676 KB` RSS; candidate was `3:00.43` wall and `176660 KB` RSS. The timing delta was noise and memory increased, so the source was reverted.
- **Outcome:** within-noise
- **Scratch worktree:** `/data/tmp/br-import-hash-reuse-candidate-20260504-MvLNe2`
- **Profile evidence:** `.beads/issues.jsonl` comment `568` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 2% to import content-hash recomputation on a 100,000-record relation-heavy import workload.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Fresh-database collision-scan elision — within-noise

- **Hypothesis:** Skipping collision scans when importing into a fresh database would reduce import CPU.
- **Workload(s) probed:** The 12,000-record fresh forced-import corpus with JSONL, status, doctor, and label-hash checks.
- **Measurement summary:** Baseline was `3:01.07` wall and `179.94s` user; candidate was `3:01.04` wall and `179.89s` user. The exact-noise probe was reverted.
- **Outcome:** within-noise
- **Scratch worktree:** `/data/tmp/br-fresh-scan-skip-candidate-20260504-nz1GdW`
- **Profile evidence:** `.beads/issues.jsonl` comment `573` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 2% to fresh-database collision scanning on a 100,000-record relation-heavy import workload.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Forced-import preflight pruning — rejected

- **Hypothesis:** Pruning JSONL issue-ID and tombstone preflight scans would reduce forced-import setup cost.
- **Workload(s) probed:** Focused planning semantics and the 12,000-record fresh forced-import corpus.
- **Measurement summary:** Baseline was `4:54.75` wall, `294.03s` user, and `148620 KB` RSS; candidate was `4:57.58` wall, `296.93s` user, and `155756 KB` RSS. Filesystem output was unchanged and the source was reverted.
- **Outcome:** rejected
- **Scratch worktree:** historical uncommitted candidate recorded on `beads_rust-72yf.5`
- **Profile evidence:** `.beads/issues.jsonl` comment `556` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 5% to JSONL issue-ID and tombstone preflight scans on an import wider than 12,000 records.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Forced-export issue batch 1024 — rejected

- **Hypothesis:** Raising `EXPORT_ISSUE_BATCH_SIZE` from 256 to 1024 would reduce forced-export batch overhead.
- **Workload(s) probed:** Forced export on the large blocked-projection fixture, with byte-identical JSONL.
- **Measurement summary:** Baseline was `64.350s +/- 0.308s`; candidate was slower at `65.829s +/- 1.084s`. The source was reverted.
- **Outcome:** rejected
- **Scratch worktree:** `/tmp/br-blocked-projection-real2-tTf4Nx`
- **Profile evidence:** `.beads/issues.jsonl` comment `478` on `beads_rust-72yf.5`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 5% to export batch setup on an export substantially larger than the measured corpus.
- **Bead id (if applicable):** `beads_rust-72yf.5`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Large-page full scan with batched relations — reverted

- **Hypothesis:** A full issue scan combined with batched relation hydration would beat the medium-page path for large structured list pages.
- **Workload(s) probed:** JSON list pages at limits 128, 150, and 200 with byte-identical output.
- **Measurement summary:** Limit 128 regressed from `270.3 ms` to `317.5 ms`, limit 150 from `291.7 ms` to `330.8 ms`, and limit 200 from `364.8 ms` to `412.0 ms`. The full-scan plus full-relation-metadata successor was retained instead.
- **Outcome:** reverted
- **Scratch worktree:** `/data/tmp/br-read-matrix-20260504-aTl0u9`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-list-large-page-fullscan/final/hyperfine.md`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.32`
- **Commit (if attempted):** uncommitted

### 2026-05-04 — Direct TOON tabular page writer — reverted

- **Hypothesis:** Direct tabular TOON serialization would avoid intermediary allocations and reduce structured list latency.
- **Workload(s) probed:** A large tabular TOON list page with byte parity.
- **Measurement summary:** Baseline was `701.4 ms +/- 8.2 ms`; direct-writer candidate was `755.7 ms +/- 5.6 ms`; an allocation-trimmed variant was noisier and slower at `785.6 ms +/- 103.7 ms`. Candidate `7c71906258b1311a8d25f67cdd086f759a9c81f3` was reverted.
- **Outcome:** reverted
- **Scratch worktree:** historical candidate recorded in the artifact
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260504T-list-toon-tabular-page/summary.json`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** `beads_rust-72yf.21`
- **Commit (if attempted):** reverted by `6d0b7f4ce01a9d91f6a743a512820f6680a0784e`

### 2026-05-03 — In-place list relation decoration — reverted

- **Hypothesis:** Decorating list rows in place would avoid relation-metadata map construction and reduce full-list JSON latency.
- **Workload(s) probed:** Full-list JSON output with byte parity.
- **Measurement summary:** Candidate `59507bec22597d49dd5ffd3d90b57af9010d448e` measured `30.7 ms +/- 7.2 ms`, flat-to-worse with outliers, and was reverted.
- **Outcome:** reverted
- **Scratch worktree:** historical committed candidate
- **Profile evidence:** Git history for `59507bec22597d49dd5ffd3d90b57af9010d448e`
- **Retry-condition predicate:** Retry only if a profiler attributes a clearly-above-noise share of at least 5% to relation-metadata map construction on the full-list JSON workload.
- **Bead id (if applicable):** not recorded
- **Commit (if attempted):** reverted by `268a2dd97cd68f98a170f570fd57c624aed5011c`

### 2026-05-03 — Blocked-cache `NOT EXISTS` rewrite — reverted

- **Hypothesis:** Rewriting the indexed `NOT IN` blocked-cache filter as `NOT EXISTS` would improve query planning or execution.
- **Workload(s) probed:** Blocked-cache query path and source/query-plan inspection.
- **Measurement summary:** The candidate's premises were false: `issue_id` is non-null and indexed, and the optimizer already specializes the `NOT IN` shape. The rewrite did not establish a measured win and was reverted.
- **Outcome:** rejected
- **Scratch worktree:** historical committed candidate
- **Profile evidence:** Git history for `0fd8611f9c2496da022053f19fd43cbd8757f393`
- **Retry-condition predicate:** Not worth retrying as a standalone patch.
- **Bead id (if applicable):** not recorded
- **Commit (if attempted):** reverted by `416efc23f72997be5f8e84c7b7f12d9aa35c4082`

### 2026-08-23 — Pending-merge open reuse in `main.rs` alone — rejected

- **Hypothesis:** Reusing the advisory pending-merge inspection's read-only database open would remove the second open observed on `ready` startup.
- **Workload(s) probed:** Current `ready --limit 0 --json` Callgrind profile plus the startup, pending-merge, and command-local open control flow at commit `05ab336d72600a253a1ad6176b65486b8ca3a9a5`.
- **Measurement summary:** The advisory receipt inspection accounts for roughly `10.7M` of `186.6M` Callgrind instructions (about `5.7%`). With both automatic sync flags disabled, Phase 2 does not preopen storage; the second connection belongs to `ready::execute`. `main.rs` cannot retain the first connection because the doctor helper owns and drops it, while the storage classifier APIs are library-private. Reimplementing the classifier in the binary would duplicate fail-closed security logic.
- **Outcome:** rejected
- **Scratch worktree:** current shared tree; read-only pass 1 made no source edit
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260824T-profile-first/baseline-ready.callgrind`
- **Retry-condition predicate:** Retry only with an expanded storage/config/doctor surface and a retention-capable, non-fallback current-schema read-only probe that returns both classification and reusable storage while preserving absent, valid, legacy, malformed, duplicate, stale-schema, symlink, and fast-open-miss behavior.
- **Bead id (if applicable):** `beads_rust-7kw0`
- **Commit (if attempted):** uncommitted; no source edit

### 2026-08-23 — Retained pending-merge connection for fast-open `ready` — reverted

- **Hypothesis:** Returning the advisory pending-merge inspector's already-open read-only storage handle and consuming it in `ready` would eliminate one complete fsqlite open without changing receipt authority or fallback behavior.
- **Workload(s) probed:** Exact tracked 970-issue database; `br --no-auto-import --no-auto-flush ready --limit 0 --json`; quiet 64-core `csd` host; 50 forward-order plus 50 reverse-order Hyperfine observations per binary; byte-identical stdout and empty stderr; matched Callgrind and one-shot `/usr/bin/time -v` probes.
- **Measurement summary:** The order-balanced baseline median was `66.797326 ms` with p95 `71.106062 ms`; the candidate median was `61.688038 ms` with p95 `66.377561 ms`, improvements of `7.6489%` and `6.6499%`. A 100,000-resample bootstrap put the candidate/baseline median ratio at `0.915933..0.934663`, excluding parity. Callgrind instructions fell from `197,179,916` to `186,230,101` (`5.55%`), and one-shot maximum RSS fell from `100,208 KiB` to `91,524 KiB` (`8.67%`). The effect was real but missed the predeclared retention requirement of at least `10%` improvement in both median and p95, so pass 3 manually reversed only the pass-2 source hunks.
- **Outcome:** reverted
- **Scratch worktree:** build source `/tmp/beads_rust_perf_20260824_pass02b` on `ts1`; A/B fixture `/tmp/beads_rust_ab_20260824_pass02` on `csd`
- **Profile evidence:** `tests/artifacts/perf/beads-perf-20260824T-profile-first/pass02-ab-forward.json`, `pass02-ab-reverse.json`, `pass02-baseline-callgrind.stderr`, `pass02-candidate-callgrind.stderr`, `pass02-baseline-time.txt`, and `pass02-candidate-time.txt`
- **Retry-condition predicate:** Retry only after a fresh profile identifies a broader or materially simpler startup connection-reuse lever plausibly clearing both `10%` gates without widening receipt authority, fallback behavior, or the command surface.
- **Bead id (if applicable):** `beads_rust-7kw0`
- **Commit (if attempted):** attempted in `1f9f43d24a050a6a2494d31f83f3e1f0759f6d32`; source hunks reversed by pass 3 pending a normal corrective commit
