# Recovery evaluation

- Build: commit 21c84f88036fb75b69f023d18de87f7b755ab2d9 (tracked changes uncommitted), release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
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
| 0 ms | stop on uncertainty | 2.9 | 3.2 | 3.6 | 3.6 | 3.3 |
| 0 ms | blind retry | 33.5 | 42.7 | 45.7 | 56.7 | 41.8 |
| 0 ms | codemode resume | 116.4 | 124.4 | 130.0 | 132.6 | 124.3 |
| 25 ms | stop on uncertainty | 3.2 | 3.5 | 4.1 | 5.3 | 3.6 |
| 25 ms | blind retry | 86.3 | 90.4 | 95.0 | 98.4 | 90.8 |
| 25 ms | codemode resume | 143.2 | 150.2 | 158.8 | 170.6 | 152.8 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 3.2 | — | — | — | — | — | 3.2 |
| 0 ms | blind retry | 3.1 | — | — | 25 | 12 | 38 | 42.7 |
| 0 ms | codemode resume | 2.4 | 11 | 38 | 24 | 29 | 55 | 124.4 |
| 25 ms | stop on uncertainty | 3.5 | — | — | — | — | — | 3.5 |
| 25 ms | blind retry | 3.4 | — | — | 26 | 58 | 86 | 90.4 |
| 25 ms | codemode resume | 2.7 | 11 | 38 | 29 | 53 | 83 | 150.2 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 43.8 / 56.2 | 43 / 55 | — | — | 24 | 13 / 23 | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 189.6 / 217.1 | 163 / 185 | 0.5 | 12 | 28 | 133 / 139 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 53.9 / 60.4 | 50 / 57 | 0.5 | 1 | 33 | 13 / 26 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 118.4 / 119.7 | 117 / 118 | — | — | 27 | 88 / 90 | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 221.4 / 232.5 | 193 / 202 | 0.6 | 12 | 31 | 157 / 161 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 123.9 / 129.3 | 120 / 126 | 0.5 | 1 | 31 | 89 / 90 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 55.6 / 76.2 | 55 / 75 | — | — | 24 | 29 / 46 | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 927.8 / 955.5 | 891 / 923 | 0.7 | 12 | 28 | 860 / 888 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 72.8 / 86.6 | 68 / 82 | 0.8 | 1 | 34 | 30 / 44 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 523.1 / 548.1 | 521 / 546 | — | — | 35 | 484 / 509 | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 934.4 / 984.8 | 897 / 946 | 1.3 | 12 | 42 | 849 / 896 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 545.2 / 565.4 | 541 / 560 | 0.8 | 1 | 34 | 506 / 524 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 149.9 / 170.2 | 13.63 | 6.25 | 119 / 129 |
| recovery-plan | 11 | 0 ms | journal, process | 8.5 / 28.6 | 0.77 | 0.35 | 0 / 18 |
| recovery-plan | 11 | 25 ms | journal, storage | 106.6 / 116.9 | 9.69 | 4.44 | 68 / 74 |
| recovery-plan | 11 | 25 ms | journal, process | 5.5 / 17.1 | 0.50 | 0.23 | 2 / 3 |
| labels | 64 | 0 ms | journal, storage | 870.0 / 895.6 | 13.59 | 6.69 | 825 / 844 |
| labels | 64 | 0 ms | journal, process | 18.4 / 36.5 | 0.29 | 0.14 | 1 / 18 |
| labels | 64 | 25 ms | journal, storage | 401.2 / 469.1 | 6.27 | 3.09 | 361 / 407 |
| labels | 64 | 25 ms | journal, process | 27.4 / 62.7 | 0.43 | 0.21 | 20 / 46 |

All correctness checks passed.
