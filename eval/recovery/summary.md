# Recovery evaluation

- Build: commit ee34e8b3d39601d654eec46a3a1db50970c99b16, release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
- Environment: macos aarch64, Apple M1 Pro, 10 logical CPUs, deno 2.9.8 (long term support, release, aarch64-apple-darwin).
- Repetitions: 15 measured after 2 discarded warmups. Strategies (A) and modes (B) run round-robin within a repetition; the starting one rotates every repetition.
- Matching parameters: concurrency 4 for every strategy and mode; fixture latency per profile; the same frozen plan and fixture seed.
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

| latency | strategy | min | p50 | p90 | max | mean |
|---:|---|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 2.4 | 2.7 | 3.3 | 6.5 | 3.0 |
| 0 ms | blind retry | 31.1 | 33.6 | 38.4 | 41.3 | 34.2 |
| 0 ms | codemode resume | 89.9 | 93.8 | 98.8 | 100.1 | 94.2 |
| 25 ms | stop on uncertainty | 2.8 | 3.1 | 3.3 | 3.4 | 3.1 |
| 25 ms | blind retry | 83.7 | 86.8 | 87.5 | 88.8 | 86.2 |
| 25 ms | codemode resume | 112.1 | 120.0 | 123.2 | 124.0 | 119.8 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 2.7 | — | — | — | — | — | 2.7 |
| 0 ms | blind retry | 2.7 | — | — | 22 | 7 | 29 | 33.6 |
| 0 ms | codemode resume | 2.2 | 9 | 19 | 22 | 27 | 49 | 93.8 |
| 25 ms | stop on uncertainty | 3.1 | — | — | — | — | — | 3.1 |
| 25 ms | blind retry | 2.9 | — | — | 25 | 58 | 82 | 86.8 |
| 25 ms | codemode resume | 1.8 | 10 | 20 | 24 | 50 | 74 | 120.0 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 46.2 / 52.9 | 45 / 52 | — | — | 22 | 14 / 25 | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 183.4 / 198.5 | 157 / 168 | 0.6 | 12 | 24 | 131 / 137 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 45.3 / 60.9 | 42 / 57 | 0.4 | 1 | 26 | 13 / 29 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 113.5 / 115.5 | 112 / 114 | — | — | 24 | 88 / 89 | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 202.0 / 214.3 | 175 / 186 | 0.6 | 12 | 24 | 150 / 161 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 116.9 / 121.1 | 114 / 118 | 0.3 | 0 | 25 | 89 / 90 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 52.1 / 67.0 | 51 / 66 | — | — | 22 | 29 / 44 | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 865.6 / 884.2 | 837 / 849 | 0.7 | 12 | 24 | 814 / 826 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 63.0 / 75.1 | 59 / 71 | 0.6 | 1 | 27 | 28 / 45 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 490.5 / 505.1 | 489 / 504 | — | — | 28 | 462 / 474 | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 864.8 / 921.9 | 829 / 883 | 0.8 | 13 | 31 | 797 / 843 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 499.6 / 522.2 | 495 / 517 | 0.6 | 1 | 29 | 464 / 490 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 141.4 / 164.0 | 12.85 | 5.89 | 118 / 124 |
| recovery-plan | 11 | 0 ms | journal, process | 6.9 / 16.4 | 0.63 | 0.29 | 0 / 13 |
| recovery-plan | 11 | 25 ms | journal, storage | 91.3 / 102.5 | 8.30 | 3.80 | 63 / 72 |
| recovery-plan | 11 | 25 ms | journal, process | 4.5 / 7.6 | 0.41 | 0.19 | 1 / 2 |
| labels | 64 | 0 ms | journal, storage | 813.2 / 829.2 | 12.71 | 6.26 | 780 / 794 |
| labels | 64 | 0 ms | journal, process | 8.4 / 28.9 | 0.13 | 0.06 | -1 / 21 |
| labels | 64 | 25 ms | journal, storage | 372.8 / 445.5 | 5.83 | 2.87 | 331 / 390 |
| labels | 64 | 25 ms | journal, process | 11.8 / 25.9 | 0.18 | 0.09 | 3 / 25 |

All correctness checks passed.
