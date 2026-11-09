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
| 0 ms | stop on uncertainty | 2.9 | 4.0 | 5.5 | 5.7 | 5.7 | 4.2 |
| 0 ms | blind retry | 8.5 | 12.7 | 29.3 | 31.4 | 31.4 | 14.9 |
| 0 ms | codemode resume | 78.4 | 85.5 | 104.7 | 119.9 | 119.9 | 88.6 |
| 25 ms | stop on uncertainty | 3.1 | 4.1 | 7.1 | 9.5 | 9.5 | 4.8 |
| 25 ms | blind retry | 59.3 | 62.8 | 65.9 | 69.0 | 69.0 | 62.7 |
| 25 ms | codemode resume | 106.3 | 116.5 | 125.6 | 144.2 | 144.2 | 117.4 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 4.0 | — | — | — | — | — | 4.0 |
| 0 ms | blind retry | 3.1 | — | — | — | 7 | 7 | 12.7 |
| 0 ms | codemode resume | 2.3 | 8 | 35 | — | 23 | 23 | 85.5 |
| 25 ms | stop on uncertainty | 4.1 | — | — | — | — | — | 4.1 |
| 25 ms | blind retry | 3.2 | — | — | — | 57 | 57 | 62.8 |
| 25 ms | codemode resume | 2.7 | 9 | 38 | — | 49 | 49 | 116.5 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p95 | dispatch p50 / p95 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p95 | facts | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 12.6 / 25.3 | 11 / 24 | — | — | — | 11 / 24 | — | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 153.0 / 180.9 | 124 / 146 | 0.7 | 11 | — | 124 / 146 | 24 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 15.3 / 30.6 | 11 / 25 | 0.6 | 1 | — | 11 / 25 | 24 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 88.8 / 94.8 | 88 / 94 | — | — | — | 88 / 94 | — | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 181.4 / 205.0 | 151 / 167 | 1.0 | 13 | — | 151 / 167 | 24 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 94.8 / 108.2 | 91 / 104 | 0.6 | 0 | — | 91 / 104 | 24 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 35.1 / 44.9 | 34 / 44 | — | — | — | 34 / 44 | — | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 778.1 / 902.4 | 744 / 871 | 0.8 | 11 | — | 744 / 871 | 130 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 44.3 / 69.6 | 40 / 64 | 0.9 | 0 | — | 39 / 64 | 130 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 463.2 / 499.7 | 462 / 498 | — | — | — | 462 / 498 | — | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 782.3 / 868.6 | 751 / 827 | 1.0 | 11 | — | 751 / 827 | 130 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 478.1 / 507.0 | 474 / 501 | 0.8 | 1 | — | 474 / 501 | 130 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p95 (ms) | per op (ms) | per fact (ms) | per commit (ms) | Δ execution p50 / p95 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 139.0 / 168.2 | 12.63 | 5.79 | 5.79 | 113 / 135 |
| recovery-plan | 11 | 0 ms | journal, process | 2.9 / 14.0 | 0.26 | 0.12 | 0.12 | 0 / 12 |
| recovery-plan | 11 | 25 ms | journal, storage | 91.4 / 113.7 | 8.31 | 3.81 | 3.81 | 59 / 81 |
| recovery-plan | 11 | 25 ms | journal, process | 4.8 / 17.7 | 0.44 | 0.20 | 0.20 | 2 / 14 |
| labels | 64 | 0 ms | journal, storage | 741.7 / 858.8 | 11.59 | 5.71 | 5.71 | 708 / 828 |
| labels | 64 | 0 ms | journal, process | 3.7 / 42.8 | 0.06 | 0.03 | 0.03 | -1 / 39 |
| labels | 64 | 25 ms | journal, storage | 320.7 / 406.8 | 5.01 | 2.47 | 2.47 | 291 / 366 |
| labels | 64 | 25 ms | journal, process | 9.0 / 41.9 | 0.14 | 0.07 | 0.07 | 5 / 33 |

All correctness checks passed.
