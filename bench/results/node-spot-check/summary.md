# Code-mode engine benchmark: node-spot-check

Driver node 26.12.1; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 0e960b1d8e10 (bench/ uncommitted). Load average at start 3.27, 2.42, 2.1.

Warm: 3 discarded + 20 measured repetitions per cell (20 for failure scenarios), engines round-robin with a rotating start, in one node process. Cold: 8 fresh node processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| pi | @earendil-works/pi-codemode 1.4.0 (quickjs-wasi 3.6.2) | enforced by Pi (deadline armed before the worker starts) | not supported; not applied | not supported; every submitted call runs at once (unbounded) | not supported for tool replies; Pi caps text()/console output at 16 MiB characters only |

Process baselines (8 samples, wall ms p50 / p95): bare `node -e ""` 23.8 / 39.2; fixture-only child (imports fixtures and harness, builds datasets, exits) 76.2 / 80.8.

## triage

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 0 | 46.8 / 50.6 | 40.1-51.5 | 0.00 | 46.8 | 42.0 / 46.0 | 44.5 / 46.1 | 229.5 / 235.5 | 38 | 8 | 126.4 | 696 | 141.1 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 0 | 30.0 / 33.1 | 26.5-33.4 | 0.28 | 29.7 | n/a | 52.8 / 53.3 | 238.4 / 240.9 | 38 | 1 | 126.4 | 696 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 730 | 825.9 / 833.2 | 814.1-833.5 | 0.00 | 825.9 | 818.0 / 823.0 | 809.0 / 820.5 | 1009.2 / 1023.9 | 38 | 8 | 126.4 | 696 | 141.3 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 545 | 603.5 / 610.6 | 587.8-610.6 | 0.54 | 602.5 | n/a | 621.9 / 627.0 | 821.2 / 830.8 | 38 | 22 | 126.4 | 696 | n/a |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| triage/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| triage/zero | codemode | trace_succeeded=38 x20 |
| triage/zero | pi | pi_ok=38 x20 |
| triage/mixed | codemode | trace_succeeded=38 x20 |
| triage/mixed | pi | pi_ok=38 x20 |
