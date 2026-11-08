# Code-mode engine benchmark: ABC

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit undefined. Load average at start 2.13, 1.7, 1.78.

Warm: 3 discarded + 20 measured repetitions per cell (20 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 0 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| cm-A | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| cm-B | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| cm-C | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |

## triage

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | cm-A | 20/20 | success x20 | 0 | 55.1 / 58.9 | 49.1-59.8 | 0.00 | 55.1 | n/a | - | - | 38 | 4 | 126.4 | 696 | n/a |
| zero | cm-B | 20/20 | success x20 | 0 | 55.1 / 58.0 | 52.2-58.6 | 0.00 | 55.1 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| zero | cm-C | 20/20 | success x20 | 0 | 51.9 / 54.4 | 46.4-59.4 | 0.00 | 51.9 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| equal | cm-A | 20/20 | success x20 | 330 | 434.5 / 443.5 | 413.1-444.4 | 0.00 | 434.5 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| equal | cm-B | 20/20 | success x20 | 330 | 436.3 / 443.8 | 414.7-444.5 | 0.00 | 436.3 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| equal | cm-C | 20/20 | success x20 | 330 | 434.9 / 441.6 | 404.1-445.9 | 0.00 | 434.9 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| mixed | cm-A | 20/20 | success x20 | 730 | 832.0 / 851.3 | 827.9-854.1 | 0.00 | 832.0 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| mixed | cm-B | 20/20 | success x20 | 730 | 839.4 / 849.5 | 818.0-850.1 | 0.00 | 839.4 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |
| mixed | cm-C | 20/20 | success x20 | 730 | 833.6 / 841.9 | 821.6-846.1 | 0.00 | 833.6 | n/a | - | - | 38 | 8 | 126.4 | 696 | n/a |

## triage-large

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | cm-A | 0/20 | output_limit (output_limit) x20 | 0 | 52.9 / 53.9 | 37.0-55.7 | 0.00 | 52.9 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| zero | cm-B | 0/20 | output_limit (output_limit) x20 | 0 | 52.6 / 54.3 | 46.1-55.3 | 0.00 | 52.6 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| zero | cm-C | 0/20 | output_limit (output_limit) x20 | 0 | 52.4 / 53.9 | 36.6-54.6 | 0.00 | 52.4 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| equal | cm-A | 0/20 | output_limit (output_limit) x20 | 210 | 89.9 / 93.2 | 85.2-94.5 | 0.00 | 89.9 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| equal | cm-B | 0/20 | output_limit (output_limit) x20 | 210 | 91.0 / 95.1 | 84.8-97.8 | 0.00 | 91.0 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| equal | cm-C | 0/20 | output_limit (output_limit) x20 | 210 | 89.0 / 93.3 | 79.1-95.0 | 0.00 | 89.0 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| mixed | cm-A | 0/20 | output_limit (output_limit) x20 | 490 | 127.0 / 130.2 | 118.6-130.4 | 0.00 | 127.0 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| mixed | cm-B | 0/20 | output_limit (output_limit) x20 | 490 | 124.8 / 130.4 | 120.0-133.5 | 0.00 | 124.8 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |
| mixed | cm-C | 0/20 | output_limit (output_limit) x20 | 490 | 126.2 / 130.0 | 109.5-130.4 | 0.00 | 126.1 | n/a | - | - | 1 | 1 | 148.2 | 0 | n/a |

## denied

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | cm-A | 20/20 | success x20 | 45 | 117.8 / 122.7 | 111.3-125.8 | 0.00 | 117.8 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| zero | cm-B | 20/20 | success x20 | 45 | 119.0 / 124.8 | 106.9-125.2 | 0.00 | 118.9 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| zero | cm-C | 20/20 | success x20 | 45 | 114.5 / 118.3 | 112.7-118.4 | 0.00 | 114.5 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| equal | cm-A | 20/20 | success x20 | 375 | 488.9 / 495.7 | 475.3-498.2 | 0.00 | 488.9 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| equal | cm-B | 20/20 | success x20 | 375 | 491.6 / 502.7 | 476.9-505.6 | 0.00 | 491.6 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| equal | cm-C | 20/20 | success x20 | 375 | 487.9 / 495.6 | 476.4-496.2 | 0.00 | 487.9 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| mixed | cm-A | 20/20 | success x20 | 765 | 888.0 / 897.4 | 878.1-897.4 | 0.00 | 888.0 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| mixed | cm-B | 20/20 | success x20 | 765 | 889.9 / 900.8 | 879.7-901.4 | 0.00 | 889.9 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |
| mixed | cm-C | 20/20 | success x20 | 765 | 886.3 / 897.9 | 872.5-900.7 | 0.00 | 886.3 | n/a | - | - | 37 | 8 | 119.1 | 715 | n/a |

## deadline

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | cm-A | 20/20 | timeout (deadline) x20 | - | 262.4 / 265.5 | 258.3-266.4 | 0.00 | 262.4 | n/a | - | - | 3 (3-4) | 1 | 23.9 | 0 | n/a |
| mixed | cm-B | 20/20 | timeout (deadline) x20 | - | 262.2 / 266.0 | 258.7-266.1 | 0.00 | 262.2 | n/a | - | - | 3 (3-4) | 1 | 23.9 | 0 | n/a |
| mixed | cm-C | 20/20 | timeout (deadline) x20 | - | 262.4 / 266.3 | 258.7-267.6 | 0.00 | 262.3 | n/a | - | - | 3 (3-4) | 1 | 23.9 | 0 | n/a |

## guest-exception

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | cm-A | 20/20 | guest_error (guest_error) x20 | - | 64.6 / 68.4 | 47.4-81.0 | 0.00 | 64.6 | n/a | - | - | 28 | 2 | 125.4 | 0 | n/a |
| zero | cm-B | 20/20 | guest_error (guest_error) x20 | - | 65.1 / 66.7 | 50.1-67.3 | 0.00 | 65.1 | n/a | - | - | 28 | 2 | 125.4 | 0 | n/a |
| zero | cm-C | 20/20 | guest_error (guest_error) x20 | - | 62.2 / 64.3 | 59.5-64.8 | 0.00 | 62.1 | n/a | - | - | 28 | 4 | 125.4 | 0 | n/a |

## lost-response

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | cm-A | 20/20 | timeout (deadline) x20 | - | 314.7 / 316.0 | 310.0-316.2 | 0.00 | 314.6 | n/a | - | - | 2 | 1 | 7.9 | 0 | n/a |
| mixed | cm-B | 20/20 | timeout (deadline) x20 | - | 314.4 / 317.8 | 310.1-319.2 | 0.00 | 314.4 | n/a | - | - | 2 | 1 | 7.9 | 0 | n/a |
| mixed | cm-C | 20/20 | timeout (deadline) x20 | - | 314.2 / 318.3 | 309.9-320.2 | 0.00 | 314.1 | n/a | - | - | 2 | 1 | 7.9 | 0 | n/a |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| triage/zero | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | cm-A | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | cm-B | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | cm-C | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | cm-A | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | cm-B | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | cm-C | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | cm-A | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | cm-B | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | cm-C | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| denied/zero | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | cm-A | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | cm-B | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | cm-C | none | 0 | 0 | 0 | 0 | none | - |
| lost-response/mixed | cm-A | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | cm-B | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | cm-C | none | 0 | 0 | 0 | 1 | none | - |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| triage/zero | cm-A | - |
| triage/zero | cm-B | - |
| triage/zero | cm-C | - |
| triage/equal | cm-A | - |
| triage/equal | cm-B | - |
| triage/equal | cm-C | - |
| triage/mixed | cm-A | - |
| triage/mixed | cm-B | - |
| triage/mixed | cm-C | - |
| triage-large/zero | cm-A | - |
| triage-large/zero | cm-B | - |
| triage-large/zero | cm-C | - |
| triage-large/equal | cm-A | - |
| triage-large/equal | cm-B | - |
| triage-large/equal | cm-C | - |
| triage-large/mixed | cm-A | - |
| triage-large/mixed | cm-B | - |
| triage-large/mixed | cm-C | - |
| denied/zero | cm-A | - |
| denied/zero | cm-B | - |
| denied/zero | cm-C | - |
| denied/equal | cm-A | - |
| denied/equal | cm-B | - |
| denied/equal | cm-C | - |
| denied/mixed | cm-A | - |
| denied/mixed | cm-B | - |
| denied/mixed | cm-C | - |
| deadline/mixed | cm-A | - |
| deadline/mixed | cm-B | - |
| deadline/mixed | cm-C | - |
| guest-exception/zero | cm-A | - |
| guest-exception/zero | cm-B | - |
| guest-exception/zero | cm-C | - |
| lost-response/mixed | cm-A | - |
| lost-response/mixed | cm-B | - |
| lost-response/mixed | cm-C | - |
