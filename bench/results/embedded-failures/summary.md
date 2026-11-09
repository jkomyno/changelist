# Code-mode engine benchmark: embedded-failures

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 6d26458c9151 (dirty outside bench/). Load average at start 2.9, 3.6, 3.67.

Warm: 0 discarded + 3 measured repetitions per cell (3 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 0 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | Deno backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-prewarm | Deno backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-embedded | embedded-v8 backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and embedded worker startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the embedded guest is capped at 1 MiB |
| codemode-embedded-prewarm | embedded-v8 backend; bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and embedded worker startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the embedded guest is capped at 1 MiB |

## deadline

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 3/3 | timeout (deadline) x3 | - | 261.3 / 264.2 | 260.7-264.2 | 0.00 | 261.2 | 253.0 / 253.0 | - | - | 4 | 1 | 23.9 | 0 | 29.3 |
| mixed | codemode-prewarm | 3/3 | timeout (deadline) x3 | - | 253.8 / 264.5 | 252.7-264.5 | 0.00 | 253.8 | 253.0 / 253.0 | - | - | 4 | 1 | 31.9 | 0 | 37.3 |
| mixed | codemode-embedded | 3/3 | timeout (deadline) x3 | - | 257.2 / 257.9 | 257.0-257.9 | 0.00 | 257.2 | 252.0 / 252.0 | - | - | 4 | 1 | 23.9 | 0 | 29.3 |
| mixed | codemode-embedded-prewarm | 3/3 | timeout (deadline) x3 | - | 253.5 / 257.2 | 253.2-257.2 | 0.00 | 253.5 | 252.0 / 252.0 | - | - | 5 (4-5) | 1 | 32.0 | 0 | 37.9 |

## guest-exception

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 3/3 | guest_error (guest_error) x3 | - | 35.2 / 35.9 | 33.3-35.9 | 0.00 | 35.2 | 28.0 / 29.0 | - | - | 28 | 7 | 125.4 | 0 | 145.4 |
| zero | codemode-prewarm | 3/3 | guest_error (guest_error) x3 | - | 14.0 / 32.8 | 11.1-32.8 | 0.00 | 14.0 | 12.0 / 26.0 | - | - | 28 | 2 | 125.4 | 0 | 145.2 |
| zero | codemode-embedded | 3/3 | guest_error (guest_error) x3 | - | 18.1 / 18.8 | 18.0-18.8 | 0.00 | 18.1 | 13.0 / 13.0 | - | - | 28 | 5 | 125.4 | 0 | 145.4 |
| zero | codemode-embedded-prewarm | 3/3 | guest_error (guest_error) x3 | - | 7.3 / 19.1 | 6.9-19.1 | 0.00 | 7.2 | 6.0 / 14.0 | - | - | 28 | 4 | 125.4 | 0 | 145.1 |

## lost-response

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 3/3 | timeout (deadline) x3 | - | 310.1 / 310.6 | 307.6-310.6 | 0.00 | 310.1 | 302.0 / 303.0 | - | - | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | codemode-prewarm | 3/3 | timeout (deadline) x3 | - | 304.1 / 310.5 | 304.1-310.5 | 0.00 | 304.1 | 303.0 / 303.0 | - | - | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | codemode-embedded | 3/3 | timeout (deadline) x3 | - | 307.6 / 307.9 | 307.3-307.9 | 0.00 | 307.6 | 302.0 / 303.0 | - | - | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | codemode-embedded-prewarm | 3/3 | timeout (deadline) x3 | - | 303.8 / 308.3 | 302.5-308.3 | 0.00 | 303.8 | 301.0 / 302.0 | - | - | 2 | 1 | 7.9 | 0 | 10.2 |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| deadline/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-embedded | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-embedded-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| lost-response/mixed | codemode | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-prewarm | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-embedded | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-embedded-prewarm | none | 0 | 0 | 0 | 1 | none | - |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| deadline/mixed | codemode | trace_succeeded=3 x3; trace_unknown=1 x3; unknown:started+abort=1 x3 |
| deadline/mixed | codemode-prewarm | trace_succeeded=3 x1; trace_unknown=1 x1; unknown:started+abort=1 x1; trace_succeeded=4 x2 |
| deadline/mixed | codemode-embedded | trace_succeeded=3 x3; trace_unknown=1 x3; unknown:started+abort=1 x3 |
| deadline/mixed | codemode-embedded-prewarm | trace_succeeded=3 x1; trace_unknown=1 x3; unknown:started+abort=1 x3; trace_succeeded=4 x2 |
| guest-exception/zero | codemode | trace_succeeded=28 x3 |
| guest-exception/zero | codemode-prewarm | trace_succeeded=28 x3 |
| guest-exception/zero | codemode-embedded | trace_succeeded=28 x3 |
| guest-exception/zero | codemode-embedded-prewarm | trace_succeeded=28 x3 |
| lost-response/mixed | codemode | trace_succeeded=1 x3; trace_unknown=1 x3; unknown:started+effect_applied+abort=1 x3 |
| lost-response/mixed | codemode-prewarm | trace_succeeded=1 x3; trace_unknown=1 x3; unknown:started+effect_applied+abort=1 x3 |
| lost-response/mixed | codemode-embedded | trace_succeeded=1 x3; trace_unknown=1 x3; unknown:started+effect_applied+abort=1 x3 |
| lost-response/mixed | codemode-embedded-prewarm | trace_succeeded=1 x3; trace_unknown=1 x3; unknown:started+effect_applied+abort=1 x3 |
