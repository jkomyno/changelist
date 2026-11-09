# Recovery evaluation

- Build: commit 5f92c92eedd8f2602966d48f2ad2a65d28aa2a78, release profile, codemode 0.1.0, rustc 1.99.1 (58cf8f9c3 2026-10-13).
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
| 0 ms | blind retry | 0/15 | 14/15 | 4 | 4–5 | 1 | 2–3 |
| 0 ms | codemode resume | 15/15 | 0/15 | 1–2 | 2 | 0 | 1 |
| 25 ms | stop on uncertainty | 0/15 | 0/15 | 5 | 0 | 0 | 0 |
| 25 ms | blind retry | 0/15 | 15/15 | 4 | 5 | 1 | 3 |
| 25 ms | codemode resume | 15/15 | 0/15 | 1 | 2 | 0 | 1 |

### Recovery time (ms, strategy start to done)

| latency | strategy | min | p50 | p90 | max | mean |
|---:|---|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 2.8 | 3.3 | 4.2 | 6.6 | 3.6 |
| 0 ms | blind retry | 8.5 | 10.7 | 19.3 | 60007.7 | 4012.0 |
| 0 ms | codemode resume | 66.7 | 73.9 | 136.3 | 10096.1 | 748.9 |
| 25 ms | stop on uncertainty | 2.7 | 3.4 | 4.1 | 4.4 | 3.5 |
| 25 ms | blind retry | 58.9 | 61.0 | 62.5 | 71.4 | 61.5 |
| 25 ms | codemode resume | 91.9 | 97.1 | 118.6 | 128.3 | 101.9 |

### Where the time goes (p50, ms)

| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 0 ms | stop on uncertainty | 3.3 | — | — | — | — | — | 3.3 |
| 0 ms | blind retry | 3.3 | — | — | — | 6 | 6 | 10.7 |
| 0 ms | codemode resume | 2.8 | 8 | 30 | — | 22 | 22 | 73.9 |
| 25 ms | stop on uncertainty | 3.4 | — | — | — | — | — | 3.4 |
| 25 ms | blind retry | 3.5 | — | — | — | 56 | 56 | 61.0 |
| 25 ms | codemode resume | 2.9 | 8 | 30 | — | 44 | 44 | 97.1 |

## B. Persistence overhead without faults

| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | runtime only | 15/15 | 11.9 / 23.1 | 11 / 22 | — | — | — | 11 / 22 | — |
| recovery-plan | 11 | 0 ms | journal, storage | 15/15 | 126.4 / 136.2 | 104 / 112 | 0.4 | 9 | — | 104 / 112 | 24 |
| recovery-plan | 11 | 0 ms | journal, process | 15/15 | 14.1 / 26.3 | 11 / 24 | 0.3 | 0 | — | 11 / 24 | 24 |
| recovery-plan | 11 | 25 ms | runtime only | 15/15 | 86.5 / 88.3 | 86 / 87 | — | — | — | 86 / 87 | — |
| recovery-plan | 11 | 25 ms | journal, storage | 15/15 | 154.3 / 160.8 | 132 / 139 | 0.4 | 9 | — | 132 / 139 | 24 |
| recovery-plan | 11 | 25 ms | journal, process | 15/15 | 89.2 / 90.0 | 86 / 88 | 0.3 | 0 | — | 86 / 88 | 24 |
| labels | 64 | 0 ms | runtime only | 15/15 | 31.2 / 44.9 | 30 / 44 | — | — | — | 30 / 44 | — |
| labels | 64 | 0 ms | journal, storage | 15/15 | 638.4 / 646.7 | 615 / 625 | 0.6 | 9 | — | 615 / 625 | 130 |
| labels | 64 | 0 ms | journal, process | 15/15 | 44.1 / 49.3 | 40 / 45 | 0.5 | 0 | — | 40 / 45 | 130 |
| labels | 64 | 25 ms | runtime only | 15/15 | 448.0 / 454.5 | 447 / 453 | — | — | — | 447 / 453 | — |
| labels | 64 | 25 ms | journal, storage | 15/15 | 656.0 / 685.0 | 630 / 661 | 0.6 | 9 | — | 630 / 661 | 130 |
| labels | 64 | 25 ms | journal, process | 15/15 | 456.5 / 472.1 | 453 / 468 | 0.5 | 0 | — | 453 / 468 | 130 |

### Journal cost against the runtime alone (paired by repetition)

| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |
|---|---:|---:|---|---:|---:|---:|---:|
| recovery-plan | 11 | 0 ms | journal, storage | 117.7 / 128.2 | 10.70 | 4.91 | 96 / 106 |
| recovery-plan | 11 | 0 ms | journal, process | 1.7 / 15.6 | 0.15 | 0.07 | 0 / 14 |
| recovery-plan | 11 | 25 ms | journal, storage | 67.7 / 74.6 | 6.15 | 2.82 | 47 / 53 |
| recovery-plan | 11 | 25 ms | journal, process | 2.0 / 4.0 | 0.18 | 0.08 | 0 / 2 |
| labels | 64 | 0 ms | journal, storage | 598.1 / 617.2 | 9.35 | 4.60 | 576 / 597 |
| labels | 64 | 0 ms | journal, process | 3.4 / 17.7 | 0.05 | 0.03 | 1 / 15 |
| labels | 64 | 25 ms | journal, storage | 209.9 / 239.5 | 3.28 | 1.61 | 185 / 217 |
| labels | 64 | 25 ms | journal, process | 9.3 / 18.1 | 0.15 | 0.07 | 7 / 16 |

## Correctness failures

- A 0 ms repetition 5: crash ledger has 9 rows (1 held), expected 10 (1 held)
- A 0 ms repetition 5 codemode resume: codemode ended incomplete with ["label-107", "comment-105"] open, expected needs_review with [comment-105]
