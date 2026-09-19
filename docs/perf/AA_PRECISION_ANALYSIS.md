# Release-latency A/A precision analysis and budget calibration

Bead: `beads_rust-zxfz.1`. Frozen 2026-09-18.

Source data: the four retained A/A shard bundles and `aa-audit.json` from the
2026-09-06 reality-check run — 28 canonical workloads, 404 raw rows and 198
retained observations per side each, four distinct boot IDs, matched-identity
sides. Because both sides are the same binary, the true effect is zero by
construction, so every bound below is the measurement's own null.

That evidence existed only in `/dev/shm/br-reality-20260906` (volatile,
RAM-backed) and the provider artifacts had expired. It is preserved at
`/data/tmp/br-zxfz1-aa-evidence-20260918/` with `SHA256SUMS.txt`. The derived
summaries are committed here; the multi-megabyte raw bundles are deliberately
not, because `beads_rust-di3tb.4` tracks repository bloat.

The reimplementation used for this analysis reproduced **all 28 recorded
`median_upper_pct` and `p95_upper_pct` values from `aa-audit.json` exactly**
(within 0.01), which is what licenses the resampling below.

## Finding 1 — the p95 leg's upper endpoint is the sample maximum

`compare_matched_runs` does not work on raw samples. `block_extrema`
(`tests/common/baseline.rs`) pairs the 198 samples into **99 ABBA blocks** and
takes each block's minimum and maximum; `quantile_ranks` is then evaluated at
`n = 99` blocks, and the reported upper bound is
`candidate_block_maxima[upper_rank - 1] - baseline_block_minima[lower_rank - 1]`.

Reproducing that exact binomial construction at `QUANTILE_TAIL_ALPHA = 1/160`:

| blocks (samples/side) | median interval | p95 interval | p95 upper endpoint |
|---|---|---|---|
| **99 (198)** | [37, 63] | **[88, 99]** | **rank 99 of 99 — the maximum** |
| 199 (398) | — | [181, 197] | quantile 0.9899 |
| 495 (990) | — | [457, 483] | quantile 0.9758 |
| 990 (1980) | — | [923, 958] | quantile 0.9677 |
| 4950 (9900) | — | [4663, 4741] | quantile 0.9578 |

At the 99 blocks this harness collects, the p95 upper endpoint is rank 99 of 99:
**the single largest observation on the candidate side**, compared against the
88th-smallest baseline block minimum. That is not a p95 comparison. The exact
interval is being honest — 99 blocks genuinely cannot bound a 0.95 quantile from
above at this α — and the consequence on real A/A data was p95 upper bounds from
**8.7% to 1029.0%** at a true zero effect.

Moving that endpoint off the sample maximum takes 199 blocks; reaching an
effective quantile near 0.976 takes 495 blocks, a five-fold sampling increase
(~10-20 hours of measurement per candidate). Convergence is slow because the
rank offset grows as sqrt(n) while the sample grows as n.

The median leg needs no such increase: its interval at 99 blocks is [37, 63] and
its A/A upper bounds ran 2.5% to 55.3%.

## Finding 2 — what the joint gate could certify, and why it was replaced

`classify` previously required **both** legs under budget to return `Pass`, so
the p95 leg set the floor. Per workload the smallest certifiable regression was
`max(median_upper_pct, p95_upper_pct)`. Under A/A a uniform budget admitted:

| uniform budget | joint gate | median leg only |
|---|---|---|
| 10% | 1 / 28 | 8 / 28 |
| 15% | 1 / 28 | 13 / 28 |
| 20% | 2 / 28 | 19 / 28 |
| 25% | 4 / 28 | 22 / 28 |
| 30% | 6 / 28 | 23 / 28 |
| 40% | 7 / 28 | 27 / 28 |
| 50% | 14 / 28 | 28 / 28 |
| 185% | 27 / 28 | — |

One workload (`10000-update-diagnostic-no-auto-flush`) needed 1029% to pass its
own null. A joint-gate budget admitting 27 of 28 workloads under A/A had to
tolerate a 185% slowdown, which is not a performance contract.

The decision (operator, 2026-09-18) was to **gate on the median leg and drop
p95 from the verdict**, keeping the p95 interval computed, serialized and
retained as evidence. Its accepted cost is asserted directly in
`tests/benchmark_comparison.rs::a_tail_only_regression_is_reported_in_evidence_but_does_not_fail_the_gate`:
**this comparator does not gate tail latency.**

Dropping p95 also removed a bound it had been supplying by accident — below 99
blocks `p95.upper` is unbounded, which forced `Inconclusive` — so
`MIN_GATING_BLOCKS = 99` now states that floor explicitly. Otherwise a
20-sample run would have begun returning `Pass`.

## Finding 3 — the blowup does not track absolute latency

The obvious hypothesis is wrong, so the `version` workloads must **not** be
excluded on the reasoning that fast commands are proportionally noisier:

- Worst p95 bound: `10000-update-diagnostic-no-auto-flush`, baseline median
  **89.8 ms**, p95 upper **1029.0%**.
- Best p95 bound: `10000-close-default-auto-flush`, baseline median
  **1434.6 ms**, p95 upper **8.7%**.
- Sub-10 ms workloads (n=4): p95 upper 44.7%-127.8%, median upper 11.2%-35.1%.
- >=100 ms workloads (n=13): p95 upper 8.7%-168.3%, median upper 2.5%-28.9%.

The ranges overlap heavily. This is per-workload tail behaviour.

## Budget calibration

A single A/A run cannot set 28 budgets with a controlled false-positive rate, so
the null distribution of the median-leg upper bound was estimated by resampling
the retained data — no new measurement.

Method: block-level exchangeability permutation. The two A/A sides are matched
per block, so under the null, swapping which side supplies block *i* is
exchangeable. For each workload, 4000 permutations (seed 20260918) independently
swap each of the 99 block pairs and recompute the comparator's own median-leg
upper bound. This preserves within-block dependence and the ABBA structure the
comparator assumes.

Frozen rule, applied mechanically with no per-workload hand adjustment:

> `budget_pct = ceil(` quantile at `1 - 0.05/28` of the permutation null `)`

That is a **5% family-wise** false-regression rate across the 28-workload
matrix (Bonferroni over 28 comparisons), not 5% per workload — which would give
a ~76% chance of at least one false regression per run.

Results are in `release_latency_budgets.json` (28 entries, 4%-85%) with the
per-workload null quantiles in `release_latency_null_study.json`. Every
workload's observed A/A bound fell inside its own permutation null, which is an
independent check that the 2026-09-06 run really was a null.

**A budget is also that workload's detection floor.** A regression smaller than
its budget cannot be distinguished from measurement noise at 99 blocks. Nine
workloads sit above 25% and `1000-update-diagnostic-no-auto-flush` is at 85%;
those are weak-power workloads, not tight contracts, and the honest way to
tighten them is more blocks, not a smaller number.

These budgets are frozen *before* any held-out A/B is opened, which is the
order the bead requires. They are committed here rather than left in the
uncommitted `vars.BR_PERF_BUDGETS_JSON` repository variable whose emptiness made
CI run 34047413559 exit 2.
