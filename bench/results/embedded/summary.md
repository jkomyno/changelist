# Code-mode engine benchmark: embedded

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 6d26458c9151 (dirty outside bench/). Load average at start 2.98, 3.33, 4.24.

Warm: 2 discarded + 15 measured repetitions per cell (15 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 5 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | Deno backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-prewarm | Deno backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-embedded | embedded-v8 backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and embedded worker startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the embedded guest is capped at 1 MiB |
| codemode-embedded-prewarm | embedded-v8 backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and embedded worker startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the embedded guest is capped at 1 MiB |

Process baselines (5 samples, wall ms p50 / p95): bare `bun -e ""` 3.5 / 4.6; fixture-only child (imports fixtures and harness, builds datasets, exits) 20.9 / 25.9.

## noop

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 15/15 (cold 5/5) | success x15 | 0 | 32.0 / 35.5 | 29.4-35.5 | 0.00 | 32.0 | 25.0 / 27.0 | 30.1 / 30.8 | 153.1 / 155.6 | 0 | 0 | 0.0 | 2 | 0.7 |
| zero | codemode-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 5.4 / 6.4 | 5.0-6.4 | 0.00 | 5.4 | 4.0 / 5.0 | 30.3 / 31.3 | 154.8 / 155.6 | 0 | 0 | 0.0 | 2 | 0.7 |
| zero | codemode-embedded | 15/15 (cold 5/5) | success x15 | 0 | 14.7 / 16.2 | 13.3-16.2 | 0.00 | 14.7 | 10.0 / 10.0 | 15.3 / 15.6 | 138.7 / 140.8 | 0 | 0 | 0.0 | 2 | 0.7 |
| zero | codemode-embedded-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 2.2 / 2.4 | 1.8-2.4 | 0.00 | 2.1 | 1.0 / 1.0 | 15.9 / 16.3 | 140.8 / 141.2 | 0 | 0 | 0.0 | 2 | 0.7 |

## triage

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 15/15 (cold 5/5) | success x15 | 0 | 37.8 / 52.6 | 32.0-52.6 | 0.00 | 37.8 | 30.0 / 46.0 | 39.1 / 39.3 | 163.9 / 164.3 | 38 | 4 | 126.4 | 696 | 153.0 |
| zero | codemode-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 12.6 / 14.9 | 11.0-14.9 | 0.00 | 12.6 | 11.0 / 13.0 | 39.1 / 41.7 | 163.8 / 164.7 | 38 | 4 | 126.4 | 696 | 152.7 |
| zero | codemode-embedded | 15/15 (cold 5/5) | success x15 | 0 | 18.9 / 22.2 | 17.9-22.2 | 0.00 | 18.9 | 14.0 / 16.0 | 23.0 / 23.0 | 147.7 / 147.9 | 38 | 3 | 126.4 | 696 | 153.0 |
| zero | codemode-embedded-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 8.0 / 9.3 | 6.3-9.3 | 0.00 | 8.0 | 7.0 / 8.0 | 23.5 / 24.4 | 148.7 / 150.6 | 38 | 6 | 126.4 | 696 | 152.7 |
| mixed | codemode | 15/15 (cold 5/5) | success x15 | 730 | 799.1 / 805.2 | 782.2-805.2 | 0.00 | 799.1 | 789.0 / 795.0 | 800.7 / 804.5 | 930.1 / 935.2 | 38 | 8 | 126.4 | 696 | 153.4 |
| mixed | codemode-prewarm | 15/15 (cold 5/5) | success x15 | 730 | 768.5 / 776.9 | 758.9-776.9 | 0.00 | 768.4 | 767.0 / 774.0 | 799.6 / 804.5 | 931.1 / 936.1 | 38 | 8 | 126.4 | 696 | 153.4 |
| mixed | codemode-embedded | 15/15 (cold 5/5) | success x15 | 730 | 773.5 / 781.7 | 770.1-781.7 | 0.00 | 773.5 | 767.0 / 772.0 | 780.1 / 783.6 | 906.2 / 912.6 | 38 | 8 | 126.4 | 696 | 153.4 |
| mixed | codemode-embedded-prewarm | 15/15 (cold 5/5) | success x15 | 730 | 761.2 / 766.7 | 754.3-766.7 | 0.00 | 761.2 | 760.0 / 764.0 | 779.6 / 784.5 | 908.6 / 914.3 | 38 | 8 | 126.4 | 696 | 153.4 |

## triage-large

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 15/15 (cold 5/5) | success x15 | 0 | 38.0 / 40.3 | 36.9-40.3 | 0.00 | 38.0 | 31.0 / 33.0 | 44.3 / 47.6 | 168.1 / 177.4 | 34 | 3 | 375.2 | 696 | 399.4 |
| zero | codemode-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 15.8 / 16.4 | 15.4-16.4 | 0.00 | 15.8 | 14.0 / 14.0 | 44.9 / 45.8 | 168.5 / 170.2 | 34 | 2 | 375.2 | 696 | 399.4 |
| zero | codemode-embedded | 15/15 (cold 5/5) | success x15 | 0 | 20.6 / 21.5 | 19.8-21.5 | 0.00 | 20.6 | 15.0 / 16.0 | 26.4 / 28.0 | 149.7 / 153.2 | 34 | 2 | 375.2 | 696 | 399.4 |
| zero | codemode-embedded-prewarm | 15/15 (cold 5/5) | success x15 | 0 | 10.4 / 11.0 | 9.9-11.0 | 0.00 | 10.4 | 9.0 / 9.0 | 27.2 / 27.6 | 151.3 / 151.4 | 34 | 2 | 375.2 | 696 | 399.1 |
| mixed | codemode | 15/15 (cold 5/5) | success x15 | 490 | 549.4 / 558.2 | 537.9-558.2 | 0.00 | 549.4 | 540.0 / 546.0 | 552.4 / 558.8 | 680.9 / 696.8 | 34 | 8 | 375.2 | 696 | 399.8 |
| mixed | codemode-prewarm | 15/15 (cold 5/5) | success x15 | 490 | 526.8 / 535.5 | 515.3-535.5 | 0.00 | 526.8 | 523.0 / 532.0 | 550.1 / 554.7 | 680.6 / 688.9 | 34 | 8 | 375.2 | 696 | 399.8 |
| mixed | codemode-embedded | 15/15 (cold 5/5) | success x15 | 490 | 529.8 / 545.7 | 520.6-545.7 | 0.00 | 529.8 | 522.0 / 533.0 | 534.1 / 535.7 | 665.0 / 666.0 | 34 | 8 | 375.2 | 696 | 399.8 |
| mixed | codemode-embedded-prewarm | 15/15 (cold 5/5) | success x15 | 490 | 517.9 / 526.4 | 508.7-526.4 | 0.00 | 517.8 | 515.0 / 523.0 | 534.6 / 535.9 | 665.5 / 668.5 | 34 | 8 | 375.2 | 696 | 399.8 |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| noop/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| noop/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| noop/zero | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| noop/zero | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| noop/zero | codemode | - |
| noop/zero | codemode-prewarm | - |
| noop/zero | codemode-embedded | - |
| noop/zero | codemode-embedded-prewarm | - |
| triage/zero | codemode | trace_succeeded=38 x15 |
| triage/zero | codemode-prewarm | trace_succeeded=38 x15 |
| triage/zero | codemode-embedded | trace_succeeded=38 x15 |
| triage/zero | codemode-embedded-prewarm | trace_succeeded=38 x15 |
| triage/mixed | codemode | trace_succeeded=38 x15 |
| triage/mixed | codemode-prewarm | trace_succeeded=38 x15 |
| triage/mixed | codemode-embedded | trace_succeeded=38 x15 |
| triage/mixed | codemode-embedded-prewarm | trace_succeeded=38 x15 |
| triage-large/zero | codemode | trace_succeeded=34 x15 |
| triage-large/zero | codemode-prewarm | trace_succeeded=34 x15 |
| triage-large/zero | codemode-embedded | trace_succeeded=34 x15 |
| triage-large/zero | codemode-embedded-prewarm | trace_succeeded=34 x15 |
| triage-large/mixed | codemode | trace_succeeded=34 x15 |
| triage-large/mixed | codemode-prewarm | trace_succeeded=34 x15 |
| triage-large/mixed | codemode-embedded | trace_succeeded=34 x15 |
| triage-large/mixed | codemode-embedded-prewarm | trace_succeeded=34 x15 |
