# Recovery evaluation

- Build: commit 6d26458c91513e0ef4838b1a750d305f97330865 (tracked changes uncommitted), release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
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
| 0 ms | stop on uncertainty | 2.5 | 3.3 | 3.8 | 3.8 | 3.3 |
| 0 ms | blind retry | 9.1 | 17.0 | 18.8 | 21.2 | 14.6 |
| 0 ms | codemode resume | 85.0 | 89.2 | 93.6 | 120.2 | 90.7 |
| 25 ms | stop on uncertainty | 2.8 | 3.5 | 4.6 | 4.6 | 3.5 |
| 25 ms | blind retry | 59.8 | 62.7 | 63.8 | 64.8 | 62.4 |
| 25 ms | codemode resume | 107.8 | 114.8 | 122.2 | 128.1 | 116.2 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 3.3 | — | — | — | — | — | 3.3 |
| 0 ms | blind retry | 3.2 | — | — | — | 13 | 13 | 17.0 |
| 0 ms | codemode resume | 2.5 | 10 | 35 | — | 24 | 24 | 89.2 |
| 25 ms | stop on uncertainty | 3.5 | — | — | — | — | — | 3.5 |
| 25 ms | blind retry | 3.2 | — | — | — | 58 | 58 | 62.7 |
| 25 ms | codemode resume | 2.8 | 10 | 37 | — | 47 | 48 | 114.8 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 15.3 / 29.2 | 14 / 28 | — | — | — | 14 / 28 | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 158.1 / 167.4 | 133 / 142 | 0.6 | 12 | — | 133 / 142 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 17.1 / 32.8 | 14 / 29 | 0.4 | 1 | — | 14 / 29 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 90.2 / 94.3 | 89 / 93 | — | — | — | 89 / 93 | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 186.8 / 194.5 | 157 / 166 | 0.6 | 12 | — | 157 / 166 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 95.0 / 96.4 | 92 / 92 | 0.4 | 0 | — | 92 / 92 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 29.2 / 49.4 | 28 / 48 | — | — | — | 28 / 48 | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 820.8 / 837.9 | 788 / 806 | 0.8 | 12 | — | 788 / 806 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 49.4 / 54.7 | 44 / 51 | 0.6 | 1 | — | 44 / 51 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 466.8 / 471.7 | 465 / 470 | — | — | — | 465 / 470 | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 804.3 / 829.3 | 770 / 796 | 0.9 | 12 | — | 769 / 796 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 475.1 / 480.4 | 471 / 476 | 0.6 | 0 | — | 471 / 476 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 144.7 / 153.1 | 13.15 | 6.03 | 120 / 126 |
| recovery-plan | 11 | 0 ms | journal, process | 3.1 / 17.3 | 0.28 | 0.13 | 2 / 15 |
| recovery-plan | 11 | 25 ms | journal, storage | 96.5 / 103.3 | 8.77 | 4.02 | 69 / 77 |
| recovery-plan | 11 | 25 ms | journal, process | 4.5 / 5.9 | 0.41 | 0.19 | 2 / 4 |
| labels | 64 | 0 ms | journal, storage | 788.7 / 810.3 | 12.32 | 6.07 | 757 / 779 |
| labels | 64 | 0 ms | journal, process | 9.2 / 27.5 | 0.14 | 0.07 | 6 / 24 |
| labels | 64 | 25 ms | journal, storage | 335.9 / 364.1 | 5.25 | 2.58 | 302 / 332 |
| labels | 64 | 25 ms | journal, process | 8.1 / 12.6 | 0.13 | 0.06 | 5 / 10 |

All correctness checks passed.
