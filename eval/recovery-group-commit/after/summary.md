# Recovery evaluation

- Build: commit d9442b644ad2e0e63c69e1bb9ebbfca1e4c5908b (tracked changes uncommitted), release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
- Environment: macos aarch64, Apple M1 Pro, 10 logical CPUs.
- Backend: native for every dispatch. Guest startup is absent (—); execution is the full native dispatch run. No program worker is started.
- Repetitions: 15 measured after 2 discarded warmups. Strategies (A) and modes (B) run round-robin within a repetition; the starting one rotates every repetition.
- Matching parameters: concurrency 4 and dispatch deadline 60000 ms for every strategy and mode; fixture latency per profile; the same frozen plan and fixture seed.
- Percentiles use the nearest-rank method. Totals are measured in µs and shown in ms; phases come from the session's own millisecond timings and the trace.

## A. Recovery after a killed run

Crashed state, identical for all strategies: 6 operations observed applied, 4 unknown (3 lost responses after the effect, 1 held before it), 1 never dispatched.

| latency | strategy | final state correct | claims complete | unresolved ops | requests sent | duplicate effects | repeats without effect |
|---:|---|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 0/15 | 0/15 | 5 | 0 | 0 | 0 |
| 0 ms | blind retry | 0/15 | 15/15 | 4 | 5 | 1 | 3 |
| 0 ms | codemode resume | 15/15 | 0/15 | 1 | 2 | 0 | 1 |
| 25 ms | stop on uncertainty | 0/15 | 0/15 | 5 | 0 | 0 | 0 |
| 25 ms | blind retry | 0/15 | 15/15 | 4 | 5 | 1 | 3 |
| 25 ms | codemode resume | 15/15 | 0/15 | 1 | 2 | 0 | 1 |

### Recovery time (ms, strategy start to done)

| latency | strategy | min | p50 | p90 | p95 | max | mean |
|---:|---|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 2.6 | 3.0 | 3.7 | 3.8 | 3.8 | 3.2 |
| 0 ms | blind retry | 7.1 | 14.2 | 15.5 | 15.9 | 15.9 | 12.4 |
| 0 ms | codemode resume | 67.7 | 73.8 | 125.9 | 130.1 | 130.1 | 84.2 |
| 25 ms | stop on uncertainty | 2.6 | 3.1 | 3.6 | 3.7 | 3.7 | 3.0 |
| 25 ms | blind retry | 58.0 | 59.6 | 60.7 | 62.0 | 62.0 | 59.5 |
| 25 ms | codemode resume | 97.1 | 101.3 | 172.8 | 196.3 | 196.3 | 112.4 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 3.0 | — | — | — | — | — | 3.0 |
| 0 ms | blind retry | 2.7 | — | — | — | 11 | 11 | 14.2 |
| 0 ms | codemode resume | 1.8 | 9 | 32 | — | 16 | 16 | 73.8 |
| 25 ms | stop on uncertainty | 3.1 | — | — | — | — | — | 3.1 |
| 25 ms | blind retry | 2.6 | — | — | — | 56 | 56 | 59.6 |
| 25 ms | codemode resume | 2.4 | 9 | 32 | — | 44 | 44 | 101.3 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p95 | dispatch p50 / p95 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p95 | facts | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 12.1 / 26.1 | 11 / 25 | — | — | — | 11 / 25 | — | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 111.5 / 172.0 | 86 / 136 | 0.6 | 11 | — | 86 / 136 | 24 | 17–19 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 15.1 / 27.8 | 12 / 24 | 0.6 | 0 | — | 12 / 24 | 24 | 19–22 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 87.3 / 122.1 | 86 / 121 | — | — | — | 86 / 121 | — | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 172.1 / 245.2 | 149 / 199 | 0.6 | 10 | — | 149 / 199 | 24 | 17–21 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 89.9 / 153.7 | 87 / 145 | 0.5 | 0 | — | 87 / 145 | 24 | 20–22 |
| labels | 64 | 0 ms | runtime only | 15/15 | 30.1 / 93.3 | 29 / 92 | — | — | — | 29 / 92 | — | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 548.6 / 941.9 | 525 / 892 | 0.8 | 11 | — | 525 / 892 | 130 | 94–102 |
| labels | 64 | 0 ms | journal, process | 15/15 | 43.8 / 92.2 | 40 / 86 | 0.7 | 0 | — | 40 / 85 | 130 | 124–128 |
| labels | 64 | 25 ms | runtime only | 15/15 | 449.6 / 536.1 | 448 / 535 | — | — | — | 448 / 535 | — | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 739.3 / 1226.4 | 715 / 1175 | 0.8 | 11 | — | 715 / 1175 | 130 | 109–127 |
| labels | 64 | 25 ms | journal, process | 15/15 | 456.1 / 595.1 | 452 / 581 | 0.8 | 0 | — | 452 / 580 | 130 | 120–128 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p95 (ms) | per op (ms) | per fact (ms) | per commit (ms) | Δ execution p50 / p95 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 99.2 / 145.9 | 9.02 | 4.13 | 5.84 | 75 / 111 |
| recovery-plan | 11 | 0 ms | journal, process | 3.4 / 15.8 | 0.31 | 0.14 | 0.18 | 1 / 13 |
| recovery-plan | 11 | 25 ms | journal, storage | 84.9 / 123.1 | 7.72 | 3.54 | 5.00 | 63 / 78 |
| recovery-plan | 11 | 25 ms | journal, process | 3.0 / 31.6 | 0.27 | 0.13 | 0.15 | 1 / 24 |
| labels | 64 | 0 ms | journal, storage | 517.8 / 848.7 | 8.09 | 3.98 | 5.51 | 492 / 800 |
| labels | 64 | 0 ms | journal, process | 4.4 / 35.2 | 0.07 | 0.03 | 0.04 | 1 / 32 |
| labels | 64 | 25 ms | journal, storage | 290.6 / 719.9 | 4.54 | 2.24 | 2.67 | 268 / 670 |
| labels | 64 | 25 ms | journal, process | 10.0 / 76.3 | 0.16 | 0.08 | 0.08 | 6 / 70 |

All correctness checks passed.
