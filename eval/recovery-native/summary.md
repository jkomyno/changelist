# Recovery evaluation

- Build: commit 81d9ca36facf20a161bb6d2144ac7ba3a9873745 (tracked changes uncommitted), release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
- Environment: macos aarch64, Apple M1 Pro, 10 logical CPUs.
- Backend: native for every dispatch. Guest startup is absent (—); execution is the full native dispatch run. No Deno executable is discovered or launched.
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

| latency | strategy | min | p50 | p90 | max | mean |
|---:|---|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 2.6 | 3.1 | 3.4 | 3.6 | 3.1 |
| 0 ms | blind retry | 7.7 | 10.6 | 15.7 | 17.5 | 12.4 |
| 0 ms | codemode resume | 66.3 | 70.9 | 75.3 | 80.3 | 71.3 |
| 25 ms | stop on uncertainty | 2.8 | 3.3 | 3.6 | 6.2 | 3.4 |
| 25 ms | blind retry | 59.7 | 60.4 | 61.1 | 61.6 | 60.4 |
| 25 ms | codemode resume | 88.6 | 94.1 | 100.9 | 105.9 | 94.4 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 3.1 | — | — | — | — | — | 3.1 |
| 0 ms | blind retry | 3.0 | — | — | — | 6 | 6 | 10.6 |
| 0 ms | codemode resume | 2.3 | 8 | 28 | — | 19 | 19 | 70.9 |
| 25 ms | stop on uncertainty | 3.3 | — | — | — | — | — | 3.3 |
| 25 ms | blind retry | 3.0 | — | — | — | 56 | 56 | 60.4 |
| 25 ms | codemode resume | 2.3 | 7 | 28 | — | 42 | 42 | 94.1 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 11.8 / 25.0 | 11 / 24 | — | — | — | 11 / 24 | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 127.7 / 143.5 | 104 / 123 | 0.4 | 9 | — | 104 / 123 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 13.9 / 14.7 | 11 / 12 | 0.3 | 0 | — | 11 / 12 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 86.4 / 88.2 | 85 / 87 | — | — | — | 85 / 87 | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 158.8 / 171.4 | 137 / 146 | 0.4 | 10 | — | 137 / 146 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 88.9 / 90.3 | 86 / 87 | 0.3 | 0 | — | 86 / 87 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 31.3 / 43.3 | 30 / 42 | — | — | — | 30 / 42 | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 642.4 / 719.1 | 619 / 693 | 0.6 | 9 | — | 619 / 693 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 45.4 / 50.1 | 42 / 46 | 0.5 | 0 | — | 42 / 46 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 447.9 / 456.4 | 447 / 455 | — | — | — | 447 / 455 | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 671.1 / 715.1 | 646 / 688 | 0.6 | 10 | — | 646 / 688 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 455.0 / 459.5 | 451 / 456 | 0.5 | 0 | — | 451 / 456 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 114.9 / 132.2 | 10.45 | 4.79 | 93 / 113 |
| recovery-plan | 11 | 0 ms | journal, process | -0.2 / 6.0 | -0.02 | -0.01 | -2 / 4 |
| recovery-plan | 11 | 25 ms | journal, storage | 71.8 / 83.9 | 6.52 | 2.99 | 52 / 61 |
| recovery-plan | 11 | 25 ms | journal, process | 2.0 / 4.1 | 0.19 | 0.09 | 0 / 3 |
| labels | 64 | 0 ms | journal, storage | 612.6 / 691.4 | 9.57 | 4.71 | 591 / 666 |
| labels | 64 | 0 ms | journal, process | 14.4 / 21.6 | 0.23 | 0.11 | 11 / 19 |
| labels | 64 | 25 ms | journal, storage | 222.4 / 265.0 | 3.48 | 1.71 | 198 / 239 |
| labels | 64 | 25 ms | journal, process | 5.5 / 11.0 | 0.09 | 0.04 | 4 / 8 |

All correctness checks passed.
