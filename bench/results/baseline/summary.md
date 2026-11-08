# Code-mode engine benchmark: baseline

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 0e960b1d8e10 (bench/ uncommitted). Load average at start 2.55, 2.02, 1.84.

Warm: 3 discarded + 20 measured repetitions per cell (20 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 8 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| codemode-c4 | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| pi | @earendil-works/pi-codemode 1.4.0 (quickjs-wasi 3.6.2) | enforced by Pi (deadline armed before the worker starts) | not supported; not applied | not supported; every submitted call runs at once (unbounded) | not supported for tool replies; Pi caps text()/console output at 16 MiB characters only |
| opencode | @opencode-ai/codemode 1.19.5 (v1.19.5, 810c0eba1a73; effect 4.0.0-beta.83, acorn 8.15.0, typescript 5.8.2) | enforced by CodeMode limits.timeoutMs (cooperative, Effect.timeoutOrElse from execute()) | enforced by CodeMode limits.maxToolCalls | fixed at 8 by the engine (TOOL_CALL_CONCURRENCY); the requested value is not applied | not applied: CodeMode's maxOutputBytes truncates the final output and does not bound tool replies |

Process baselines (8 samples, wall ms p50 / p95): bare `bun -e ""` 3.2 / 10.1; fixture-only child (imports fixtures and harness, builds datasets, exits) 20.3 / 36.0.

## triage

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 0 | 44.4 / 50.6 | 35.1-52.5 | 0.00 | 44.4 | 40.0 / 45.0 | 44.3 / 45.6 | 170.6 / 171.9 | 38 | 5 | 126.4 | 696 | 141.1 |
| zero | codemode-c4 | 20/20 (cold 8/8) | success x20 | 0 | 42.0 / 46.6 | 36.9-48.2 | 0.00 | 41.9 | 37.0 / 41.0 | 45.4 / 46.5 | 172.4 / 174.0 | 38 | 2 | 126.4 | 696 | 141.1 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 0 | 25.1 / 26.6 | 19.4-27.1 | 0.08 | 24.9 | n/a | 48.8 / 51.3 | 178.2 / 178.5 | 38 | 1 | 126.4 | 696 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 0 | 14.0 / 17.0 | 8.6-17.9 | 0.14 | 13.8 | n/a | 196.3 / 201.1 | 326.6 / 330.0 | 38 | 8 | 126.4 | 696 | n/a |
| equal | codemode | 20/20 (cold 8/8) | success x20 | 330 | 414.7 / 430.4 | 388.4-440.8 | 0.00 | 414.6 | 407.0 / 420.0 | 410.9 / 421.4 | 542.2 / 559.2 | 38 | 8 | 126.4 | 696 | 141.3 |
| equal | codemode-c4 | 20/20 (cold 8/8) | success x20 | 450 | 551.9 / 560.5 | 515.8-566.0 | 0.00 | 551.9 | 540.0 / 552.0 | 546.6 / 561.9 | 685.8 / 707.0 | 38 | 4 | 126.4 | 696 | 141.3 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 240 | 294.7 / 299.0 | 275.0-305.9 | 0.17 | 294.2 | n/a | 308.9 / 311.4 | 451.3 / 462.2 | 38 | 22 | 126.4 | 696 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 330 | 380.9 / 390.0 | 363.7-392.8 | 0.29 | 380.6 | n/a | 560.8 / 566.4 | 703.5 / 714.4 | 38 | 8 | 126.4 | 696 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 730 | 821.9 / 828.7 | 784.0-831.3 | 0.00 | 821.8 | 814.0 / 820.0 | 814.8 / 822.4 | 952.8 / 958.2 | 38 | 8 | 126.4 | 696 | 141.3 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | success x20 | 915 | 1018.2 / 1032.2 | 989.2-1033.5 | 0.00 | 1018.1 | 1010.0 / 1022.0 | 1021.6 / 1025.5 | 1157.5 / 1165.0 | 38 | 4 | 126.4 | 696 | 141.3 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 545 | 599.4 / 610.3 | 583.2-611.5 | 0.15 | 599.3 | n/a | 619.8 / 625.9 | 765.3 / 774.1 | 38 | 22 | 126.4 | 696 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 730 | 780.3 / 792.7 | 760.7-795.2 | 0.23 | 780.0 | n/a | 965.6 / 972.0 | 1118.8 / 1137.0 | 38 | 8 | 126.4 | 696 | n/a |

## triage-large

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 0 | 34.8 / 42.0 | 26.3-46.4 | 0.00 | 34.8 | 31.0 / 37.0 | 36.8 / 49.7 | 162.4 / 175.8 | 1 | 1 | 148.2 | 0 | 2.9 |
| zero | codemode-c4 | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 0 | 36.7 / 43.8 | 29.1-60.4 | 0.00 | 36.7 | 32.0 / 39.0 | 37.1 / 37.8 | 162.7 / 165.3 | 1 | 1 | 148.2 | 0 | 2.9 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 0 | 28.7 / 30.7 | 20.6-32.6 | 0.07 | 28.6 | n/a | 53.3 / 55.9 | 181.1 / 183.5 | 34 | 1 | 375.2 | 696 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 0 | 13.2 / 16.1 | 8.5-16.8 | 0.12 | 13.0 | n/a | 196.9 / 200.1 | 326.5 / 329.7 | 34 | 8 | 375.2 | 696 | n/a |
| equal | codemode | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 210 | 74.4 / 81.4 | 61.8-83.0 | 0.00 | 74.4 | 70.0 / 74.0 | 71.7 / 73.1 | 204.7 / 207.2 | 1 | 1 | 148.2 | 0 | 2.9 |
| equal | codemode-c4 | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 330 | 72.4 / 80.5 | 59.7-86.6 | 0.00 | 72.4 | 68.0 / 74.0 | 70.5 / 72.8 | 197.1 / 209.5 | 1 | 1 | 148.2 | 0 | 2.9 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 120 | 162.9 / 167.1 | 148.9-167.3 | 0.10 | 162.8 | n/a | 181.8 / 182.9 | 317.4 / 321.4 | 34 | 22 | 375.2 | 696 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 210 | 246.7 / 257.7 | 231.9-260.1 | 0.20 | 246.6 | n/a | 429.2 / 432.6 | 568.6 / 573.4 | 34 | 8 | 375.2 | 696 | n/a |
| mixed | codemode | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 490 | 103.5 / 112.3 | 91.8-112.5 | 0.00 | 103.4 | 99.0 / 107.0 | 101.8 / 103.3 | 236.3 / 238.8 | 1 | 1 | 148.2 | 0 | 2.9 |
| mixed | codemode-c4 | 0/20 (cold 0/8) | output_limit (output_limit) x20 | 675 | 100.5 / 111.5 | 89.0-113.3 | 0.00 | 100.5 | 96.0 / 104.0 | 101.8 / 103.5 | 234.1 / 236.7 | 1 | 1 | 148.2 | 0 | 2.9 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 305 | 348.2 / 358.0 | 334.8-359.1 | 0.08 | 348.1 | n/a | 368.8 / 374.9 | 507.8 / 518.1 | 34 | 22 | 375.2 | 696 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 490 | 522.8 / 534.5 | 509.1-535.6 | 0.16 | 522.5 | n/a | 710.6 / 720.1 | 854.9 / 859.2 | 34 | 8 | 375.2 | 696 | n/a |

Known limitation (codemode): Each listing page (~148 KiB) exceeds the 64 KiB per-frame limit between Rust and the Deno guest; failure is a measured limitation, not a harness error.

## denied

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 45 | 91.0 / 108.1 | 83.4-157.7 | 0.00 | 91.0 | 88.0 / 102.0 | 97.3 / 98.5 | 225.0 / 234.1 | 37 | 8 | 119.1 | 715 | 134.0 |
| zero | codemode-c4 | 20/20 (cold 8/8) | success x20 | 90 | 139.1 / 210.2 | 136.0-237.9 | 0.00 | 139.1 | 136.0 / 200.0 | 149.7 / 151.7 | 279.3 / 282.8 | 37 | 4 | 119.1 | 715 | 134.0 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 15 | 36.9 / 66.0 | 34.9-91.0 | 0.05 | 36.8 | n/a | 67.5 / 68.4 | 202.9 / 206.1 | 37 | 22 | 119.1 | 715 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 45 | 60.5 / 73.9 | 57.1-74.2 | 0.08 | 60.5 | n/a | 251.3 / 253.0 | 382.4 / 387.7 | 37 | 8 | 119.1 | 715 | n/a |
| equal | codemode | 20/20 (cold 8/8) | success x20 | 375 | 463.5 / 477.6 | 438.0-489.3 | 0.00 | 463.4 | 456.0 / 471.0 | 465.8 / 469.6 | 597.1 / 611.4 | 37 | 8 | 119.1 | 715 | 134.1 |
| equal | codemode-c4 | 20/20 (cold 8/8) | success x20 | 525 | 623.4 / 643.5 | 603.5-649.9 | 0.00 | 623.4 | 616.0 / 640.0 | 629.5 / 635.2 | 768.2 / 776.3 | 37 | 4 | 119.1 | 715 | 134.1 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 255 | 307.7 / 315.8 | 294.0-318.2 | 0.13 | 307.5 | n/a | 327.1 / 328.7 | 472.7 / 477.0 | 37 | 22 | 119.1 | 715 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 375 | 425.2 / 441.9 | 412.3-449.8 | 0.19 | 425.0 | n/a | 617.8 / 625.5 | 757.3 / 768.8 | 37 | 8 | 119.1 | 715 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 765 | 863.3 / 878.6 | 827.5-1009.1 | 0.00 | 863.3 | 854.0 / 869.0 | 860.9 / 881.5 | 994.6 / 1020.6 | 37 | 8 | 119.1 | 715 | 134.1 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | success x20 | 1010 | 1119.2 / 1138.0 | 1081.3-1146.4 | 0.00 | 1119.2 | 1111.0 / 1134.0 | 1121.6 / 1140.8 | 1255.9 / 1280.2 | 37 | 4 | 119.1 | 715 | 134.2 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 560 | 615.6 / 627.9 | 593.6-630.1 | 0.12 | 615.3 | n/a | 636.5 / 645.7 | 778.6 / 794.9 | 37 | 22 | 119.1 | 715 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 765 | 831.7 / 839.8 | 800.5-850.2 | 0.22 | 831.5 | n/a | 1021.9 / 1038.7 | 1161.3 / 1178.9 | 37 | 8 | 119.1 | 715 | n/a |

## deadline

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 265.1 / 268.6 | 256.7-269.9 | 0.00 | 265.1 | 257.0 / 260.0 | 266.2 / 269.8 | 398.1 / 408.7 | 4 | 1 | 23.9 | 0 | 27.7 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 266.5 / 268.7 | 260.1-269.2 | 0.00 | 266.5 | 258.0 / 260.0 | 266.5 / 270.8 | 402.7 / 412.7 | 4 | 1 | 23.9 | 0 | 27.7 |
| mixed | pi | 20/20 (cold 8/8) | timeout (timeout) x20 | - | 252.7 / 253.3 | 250.4-256.3 | 0.14 | 252.4 | n/a | 263.6 / 264.5 | 408.1 / 413.8 | 4 | 1 | 23.9 | 0 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | timeout (TimeoutExceeded) x20 | - | 258.6 / 261.6 | 254.0-267.4 | 0.25 | 258.3 | n/a | 433.0 / 439.3 | 581.3 / 586.8 | 4 | 1 | 23.9 | 0 | n/a |

## guest-exception

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | guest_error (guest_error) x20 | - | 49.5 / 58.5 | 36.0-59.8 | 0.00 | 49.4 | 44.0 / 49.0 | 45.3 / 50.3 | 171.4 / 182.8 | 28 | 2 | 125.4 | 0 | 136.5 |
| zero | codemode-c4 | 20/20 (cold 8/8) | guest_error (guest_error) x20 | - | 44.1 / 49.0 | 36.1-53.0 | 0.00 | 44.1 | 39.0 / 45.0 | 45.0 / 49.0 | 171.8 / 172.8 | 28 | 2 | 125.4 | 0 | 136.5 |
| zero | pi | 20/20 (cold 8/8) | guest_error (script) x20 | - | 24.4 / 25.9 | 18.1-26.6 | 0.06 | 24.3 | n/a | 48.5 / 51.7 | 176.7 / 179.7 | 28 | 1 | 125.4 | 0 | n/a |
| zero | opencode | 20/20 (cold 8/8) | guest_error (ExecutionFailure) x20 | - | 8.7 / 9.8 | 5.2-13.8 | 0.11 | 8.6 | n/a | 190.9 / 194.4 | 318.6 / 322.9 | 28 | 8 | 125.4 | 0 | n/a |

## lost-response

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 314.4 / 318.4 | 311.9-319.8 | 0.00 | 314.3 | 306.0 / 309.0 | 317.0 / 319.9 | 452.6 / 457.4 | 2 | 1 | 7.9 | 0 | 9.3 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 314.0 / 316.9 | 305.4-317.4 | 0.00 | 313.9 | 306.0 / 309.0 | 317.1 / 319.7 | 457.4 / 460.4 | 2 | 1 | 7.9 | 0 | 9.3 |
| mixed | pi | 20/20 (cold 8/8) | timeout (timeout) x20 | - | 302.8 / 306.3 | 301.1-311.9 | 0.12 | 302.6 | n/a | 314.2 / 316.9 | 457.0 / 465.6 | 2 | 1 | 7.9 | 0 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | timeout (TimeoutExceeded) x20 | - | 310.3 / 314.0 | 303.8-314.6 | 0.20 | 309.6 | n/a | 479.4 / 483.7 | 624.8 / 635.8 | 2 | 1 | 7.9 | 0 | n/a |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| triage/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-c4 | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | codemode | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | codemode-c4 | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-c4 | expected success, got output_limit: output_limit (x20) | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| lost-response/mixed | codemode | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-c4 | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | pi | none | 0 | 0 | 0 | 1 | none | Pi marks tracker.add_label cancelled; the ledger shows its effect applied (started+effect_applied+abort) (x20) |
| lost-response/mixed | opencode | none | 0 | 0 | 0 | 1 | none | OpenCode reports no outcome for tracker.add_label; the ledger shows its effect applied (started+effect_applied+abort) (x20) |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| triage/zero | codemode | trace_succeeded=38 x20 |
| triage/zero | codemode-c4 | trace_succeeded=38 x20 |
| triage/zero | pi | pi_ok=38 x20 |
| triage/zero | opencode | admitted=38 x20; hook_started=38 x20 |
| triage/equal | codemode | trace_succeeded=38 x20 |
| triage/equal | codemode-c4 | trace_succeeded=38 x20 |
| triage/equal | pi | pi_ok=38 x20 |
| triage/equal | opencode | admitted=38 x20; hook_started=38 x20 |
| triage/mixed | codemode | trace_succeeded=38 x20 |
| triage/mixed | codemode-c4 | trace_succeeded=38 x20 |
| triage/mixed | pi | pi_ok=38 x20 |
| triage/mixed | opencode | admitted=38 x20; hook_started=38 x20 |
| triage-large/zero | codemode | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/zero | codemode-c4 | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/zero | pi | pi_ok=34 x20 |
| triage-large/zero | opencode | admitted=34 x20; hook_started=34 x20 |
| triage-large/equal | codemode | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/equal | codemode-c4 | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/equal | pi | pi_ok=34 x20 |
| triage-large/equal | opencode | admitted=34 x20; hook_started=34 x20 |
| triage-large/mixed | codemode | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/mixed | codemode-c4 | trace_result_too_large=1 x20; result_too_large:started+complete=1 x20 |
| triage-large/mixed | pi | pi_ok=34 x20 |
| triage-large/mixed | opencode | admitted=34 x20; hook_started=34 x20 |
| denied/zero | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/zero | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/zero | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/zero | opencode | admitted=37 x20; hook_started=37 x20 |
| denied/equal | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/equal | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/equal | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/equal | opencode | admitted=37 x20; hook_started=37 x20 |
| denied/mixed | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/mixed | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/mixed | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/mixed | opencode | admitted=37 x20; hook_started=37 x20 |
| deadline/mixed | codemode | trace_succeeded=3 x20; trace_unknown=1 x20; unknown:started+abort=1 x20 |
| deadline/mixed | codemode-c4 | trace_succeeded=3 x20; trace_unknown=1 x20; unknown:started+abort=1 x20 |
| deadline/mixed | pi | pi_ok=3 x20; pi_cancelled=1 x20; cancelled:started+abort=1 x20 |
| deadline/mixed | opencode | admitted=4 x20; hook_started=4 x20; interrupted:started+abort=1 x20 |
| guest-exception/zero | codemode | trace_succeeded=28 x20 |
| guest-exception/zero | codemode-c4 | trace_succeeded=28 x20 |
| guest-exception/zero | pi | pi_ok=28 x20 |
| guest-exception/zero | opencode | admitted=28 x20; hook_started=28 x20 |
| lost-response/mixed | codemode | trace_succeeded=1 x20; trace_unknown=1 x20; unknown:started+effect_applied+abort=1 x20 |
| lost-response/mixed | codemode-c4 | trace_succeeded=1 x20; trace_unknown=1 x20; unknown:started+effect_applied+abort=1 x20 |
| lost-response/mixed | pi | pi_ok=1 x20; pi_cancelled=1 x20; cancelled:started+effect_applied+abort=1 x20 |
| lost-response/mixed | opencode | admitted=2 x20; hook_started=2 x20; interrupted:started+effect_applied+abort=1 x20 |
