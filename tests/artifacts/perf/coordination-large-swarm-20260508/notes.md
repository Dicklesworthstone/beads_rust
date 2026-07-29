# Coordination Large Swarm Evidence

Generated at: 2026-05-08T16:26:50.995844467+00:00

Corpus: 100000 issues, 20013 in_progress claims, 40065 dependencies, 250063 labels, 50462 comments, 10000 simulated agents.

Load strategy: direct_sqlite_seed, used to isolate coordination read-path measurement from JSONL import cost; sync import and doctor health flags were not run for this artifact.

Snapshots: 10000 agents, 4000 active reservations, 4000 expired reservations.

Commands:

- JSON: `/data/projects/beads_rust/.rch-target/debug/br coordination status --json --no-auto-import --no-auto-flush --owner-kind swarm-agent --comments 1 --reservations /data/projects/beads_rust/tests/artifacts/perf/coordination-large-swarm-20260508/coordination-reservations.jsonl --agents /data/projects/beads_rust/tests/artifacts/perf/coordination-large-swarm-20260508/coordination-agents.jsonl`
- TOON: `/data/projects/beads_rust/.rch-target/debug/br coordination status --format toon --no-auto-import --no-auto-flush --owner-kind swarm-agent --comments 1 --reservations /data/projects/beads_rust/tests/artifacts/perf/coordination-large-swarm-20260508/coordination-reservations.jsonl --agents /data/projects/beads_rust/tests/artifacts/perf/coordination-large-swarm-20260508/coordination-agents.jsonl`

Results:

- JSON duration: 340507 ms; output bytes: 37634197; raw sha256: 492c0ab428d3818af08bfaefbaa393deafdd557b296ae712ea913e418e5bd659; normalized sha256: f58ecdef548280fcc272ef9673ca6c75c27dc9a7cec8823e92b2cf619cbff737; peak RSS bytes: 345931776
- TOON duration: 340980 ms; output bytes: 41191574; raw sha256: 7809f42060876649518e80f85ff98bb4c077e9f7006a6651157d2d87413a051d; normalized sha256: ae3157cbc13319fc4021fea45c895348c1c35d8b72238a08a2b874e7cbf4775f; peak RSS bytes: 355753984
- Semantic hashes match: false

Audit note: the recorded semantic-hash comparison used TOON decoding without
safe folded-key expansion, so dotted fields such as `reservation.state` were
not normalized back to nested JSON before hashing. The command timings, output
sizes, raw hashes, and reservation-match counts remain the measured evidence
for this run; future reruns use safe path expansion in the benchmark
normalizer.

Guardrail: coordination status requested 1 latest comment row per in-progress issue. The command-side upper bound was 20013 comment rows for 20013 claims, while the generated corpus contained 50462 total comments. Future changes should keep the command on bounded latest-comment evidence unless they also publish a stronger measured baseline.

Reproduce: `BR_E2E_STRESS=1 BR_SYNTHETIC_SEED=20260508 BR_SYNTHETIC_EVIDENCE_ISSUES=100000 cargo test --test bench_synthetic_scale stress_coordination_large_swarm_evidence -- --ignored --nocapture`
