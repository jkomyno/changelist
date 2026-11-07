# Dispatch policy evaluation

- Build: commit d16dbc7dbc8a097e0a3d41e1cdb5d83c1999717b, release profile, codemode 0.1.0.
- Environment: macos aarch64, Apple M1 Pro, 10 logical CPUs, deno 2.9.8 (long term support, release, aarch64-apple-darwin).
- Repetitions: 21 measured per fixture and policy, after 2 discarded warmup repetitions. Policies run round-robin within each repetition; the starting policy rotates every repetition.
- Shared budget for every policy: timeout 5000 ms unless a fixture overrides it, max_calls 32, max_output_bytes 262144.
- Timeout overrides (same for every policy): deadline 540 ms.
- Concurrency cap: sequential 1 (rolling dispatch with one slot), batch 4, rolling 4.
- *Tool span*: earliest dispatch to the last observed result; shown only for runs that did not hit the deadline and have no unknown calls. *Elapsed*: the whole run, including Deno startup. *Queue wait Σ*: per-run sum over all calls of the time from admission to dispatch, or to settlement for calls never dispatched.
- Percentiles use the nearest-rank method. Timestamps are whole milliseconds (floored) and Tokio sleeps overshoot by about 1 ms, so differences of 2 ms or less are not resolvable.
- *Model* is an ideal zero-overhead makespan computed from manifest latencies; it is a model, not a measurement, shown only where every call is submitted together and the run does not end early. *p50 − model* includes timer rounding and overshoot, not only host overhead.
- Note on fail-fast: per-policy counts follow from call order: the failure frees a slot that rolling refills at once, while batch waits for its whole batch; they assume the guest reports its error within the 120 ms that the other dispatched calls take.
- Note on deadline: reported, not asserted, because the split depends on startup. With 0 to about 130 ms from run start to first dispatch, expect sequential unknown=1, cancelled_before_dispatch=11; batch succeeded=3, unknown=1, cancelled_before_dispatch=8; rolling succeeded=6, unknown=4, cancelled_before_dispatch=2. All calls are admitted within startup, far ahead of the deadline.

## Tool span (ms)

| fixture | policy | cap | model | min | p50 | p90 | max | mean | p50 − model |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-latency | sequential | 1 | 550 | 565 | 570 | 575 | 581 | 570.7 | +20 |
| mixed-latency | batch | 4 | 400 | 402 | 404 | 406 | 410 | 404.4 | +4 |
| mixed-latency | rolling | 4 | 225 | 227 | 230 | 231 | 240 | 229.8 | +5 |
| equal-latency | sequential | 1 | 400 | 415 | 420 | 424 | 434 | 420.4 | +20 |
| equal-latency | batch | 4 | 100 | 102 | 104 | 105 | 106 | 104.0 | +4 |
| equal-latency | rolling | 4 | 100 | 103 | 104 | 104 | 109 | 104.1 | +4 |
| partial-failure | sequential | 1 | 330 | 338 | 342 | 348 | 354 | 343.1 | +12 |
| partial-failure | batch | 4 | 240 | 243 | 244 | 246 | 248 | 244.7 | +4 |
| partial-failure | rolling | 4 | 120 | 121 | 122 | 123 | 125 | 122.1 | +2 |
| fail-fast | sequential | 1 | — | 243 | 245 | 247 | 250 | 245.5 | — |
| fail-fast | batch | 4 | — | 121 | 122 | 122 | 126 | 122.1 | — |
| fail-fast | rolling | 4 | — | 121 | 122 | 123 | 123 | 122.0 | — |
| deadline | sequential | 1 | — | — | — | — | — | — | — |
| deadline | batch | 4 | — | — | — | — | — | — | — |
| deadline | rolling | 4 | — | — | — | — | — | — | — |

## Elapsed (ms)

| fixture | policy | timeout | min | p50 | p90 | max | mean | startup p50 / max | queue wait Σ p50 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-latency | sequential | 5000 | 613 | 635 | 649 | 664 | 635.1 | 51 / 81 | 2520 |
| mixed-latency | batch | 5000 | 442 | 465 | 479 | 489 | 466.5 | 52 / 68 | 808 |
| mixed-latency | rolling | 5000 | 267 | 294 | 302 | 309 | 292.6 | 56 / 67 | 135 |
| equal-latency | sequential | 5000 | 456 | 474 | 483 | 489 | 473.4 | 44 / 57 | 1462 |
| equal-latency | batch | 5000 | 133 | 156 | 168 | 173 | 156.6 | 47 / 66 | 208 |
| equal-latency | rolling | 5000 | 140 | 152 | 165 | 177 | 153.4 | 44 / 60 | 208 |
| partial-failure | sequential | 5000 | 383 | 396 | 410 | 412 | 397.8 | 46 / 58 | 927 |
| partial-failure | batch | 5000 | 287 | 303 | 311 | 331 | 303.1 | 49 / 73 | 244 |
| partial-failure | rolling | 5000 | 147 | 175 | 191 | 197 | 174.9 | 45 / 71 | 32 |
| fail-fast | sequential | 5000 | 277 | 292 | 299 | 301 | 291.2 | 45 / 55 | 901 |
| fail-fast | batch | 5000 | 158 | 180 | 187 | 195 | 177.2 | 56 / 73 | 8 |
| fail-fast | rolling | 5000 | 152 | 173 | 177 | 190 | 170.7 | 51 / 68 | 6 |
| deadline | sequential | 540 | 544 | 550 | 553 | 562 | 549.9 | 51 / 63 | 5390 |
| deadline | batch | 540 | 543 | 549 | 553 | 555 | 548.8 | 55 / 98 | 3888 |
| deadline | rolling | 540 | 542 | 549 | 552 | 556 | 548.6 | 53 / 62 | 2804 |

## Correctness and outcomes

| fixture | policy | correct | peak active | call outcomes (runs) |
|---|---|---:|---:|---|
| mixed-latency | sequential | 21/21 | 1 | succeeded=8 (21/21) |
| mixed-latency | batch | 21/21 | 4 | succeeded=8 (21/21) |
| mixed-latency | rolling | 21/21 | 4 | succeeded=8 (21/21) |
| equal-latency | sequential | 21/21 | 1 | succeeded=8 (21/21) |
| equal-latency | batch | 21/21 | 4 | succeeded=8 (21/21) |
| equal-latency | rolling | 21/21 | 4 | succeeded=8 (21/21) |
| partial-failure | sequential | 21/21 | 1 | provider_failed=1, succeeded=5 (21/21) |
| partial-failure | batch | 21/21 | 4 | provider_failed=1, succeeded=5 (21/21) |
| partial-failure | rolling | 21/21 | 4 | provider_failed=1, succeeded=5 (21/21) |
| fail-fast | sequential | 21/21 | 1 | cancelled_before_dispatch=5, provider_failed=1, succeeded=2 (21/21) |
| fail-fast | batch | 21/21 | 4 | cancelled_before_dispatch=4, provider_failed=1, succeeded=3 (21/21) |
| fail-fast | rolling | 21/21 | 4 | cancelled_before_dispatch=3, provider_failed=1, succeeded=4 (21/21) |
| deadline | sequential | 21/21 | 1 | cancelled_before_dispatch=11, unknown=1 (21/21) |
| deadline | batch | 21/21 | 4 | cancelled_before_dispatch=8, succeeded=3, unknown=1 (21/21) |
| deadline | rolling | 21/21 | 4 | cancelled_before_dispatch=2, succeeded=6, unknown=4 (21/21) |

All correctness checks passed.
