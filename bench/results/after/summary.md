# Code-mode engine benchmark: after

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 3f7e5ab0da5b (dirty outside bench/). Load average at start 2.58, 2.08, 1.93.

Warm: 3 discarded + 20 measured repetitions per cell (20 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 8 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-c4 | bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| codemode-prewarm | bridge protocol 2 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB |
| pi | @earendil-works/pi-codemode 1.4.0 (quickjs-wasi 3.6.2) | enforced by Pi (deadline armed before the worker starts) | not supported; not applied | not supported; every submitted call runs at once (unbounded) | not supported for tool replies; Pi caps text()/console output at 16 MiB characters only |
| opencode | @opencode-ai/codemode 1.19.5 (v1.19.5, 810c0eba1a73; effect 4.0.0-beta.83, acorn 8.15.0, typescript 5.8.2) | enforced by CodeMode limits.timeoutMs (cooperative, Effect.timeoutOrElse from execute()) | enforced by CodeMode limits.maxToolCalls | fixed at 8 by the engine (TOOL_CALL_CONCURRENCY); the requested value is not applied | not applied: CodeMode's maxOutputBytes truncates the final output and does not bound tool replies |

Process baselines (8 samples, wall ms p50 / p95): bare `bun -e ""` 3.2 / 8.4; fixture-only child (imports fixtures and harness, builds datasets, exits) 20.4 / 33.6.

## triage

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 0 | 40.7 / 44.8 | 31.6-49.1 | 0.00 | 40.6 | 33.0 / 38.0 | 36.9 / 37.1 | 162.6 / 163.0 | 38 | 4 | 126.4 | 696 | 153.0 |
| zero | codemode-c4 | 20/20 (cold 8/8) | success x20 | 0 | 39.6 / 43.4 | 30.9-44.2 | 0.00 | 39.6 | 32.0 / 36.0 | 37.8 / 38.7 | 163.4 / 166.0 | 38 | 3 | 126.4 | 696 | 153.0 |
| zero | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 0 | 14.1 / 16.0 | 10.5-16.1 | 0.00 | 14.0 | 13.0 / 14.0 | 37.5 / 39.7 | 165.0 / 167.6 | 38 | 2 | 126.4 | 696 | 152.7 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 0 | 23.1 / 27.1 | 18.2-27.2 | 0.08 | 22.9 | n/a | 49.2 / 50.8 | 178.1 / 181.4 | 38 | 1 | 126.4 | 696 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 0 | 13.5 / 16.4 | 9.5-16.7 | 0.12 | 13.4 | n/a | 194.5 / 197.7 | 324.4 / 326.4 | 38 | 8 | 126.4 | 696 | n/a |
| equal | codemode | 20/20 (cold 8/8) | success x20 | 330 | 408.7 / 428.8 | 384.7-429.0 | 0.00 | 408.7 | 397.0 / 413.0 | 405.8 / 411.6 | 541.1 / 549.5 | 38 | 8 | 126.4 | 696 | 153.4 |
| equal | codemode-c4 | 20/20 (cold 8/8) | success x20 | 450 | 537.3 / 556.6 | 526.7-558.6 | 0.00 | 537.3 | 529.0 / 542.0 | 532.7 / 543.1 | 668.0 / 681.5 | 38 | 4 | 126.4 | 696 | 153.4 |
| equal | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 330 | 385.4 / 401.9 | 371.6-402.8 | 0.00 | 385.4 | 383.0 / 399.0 | 403.7 / 412.4 | 535.3 / 540.9 | 38 | 8 | 126.4 | 696 | 153.4 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 240 | 284.9 / 299.5 | 276.7-302.4 | 0.07 | 284.8 | n/a | 307.5 / 311.9 | 448.5 / 458.8 | 38 | 22 | 126.4 | 696 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 330 | 380.0 / 395.9 | 364.2-397.3 | 0.25 | 379.9 | n/a | 559.2 / 585.5 | 701.7 / 724.7 | 38 | 8 | 126.4 | 696 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 730 | 816.9 / 823.5 | 804.3-824.6 | 0.00 | 816.8 | 805.0 / 812.0 | 808.4 / 812.5 | 943.9 / 952.2 | 38 | 8 | 126.4 | 696 | 153.5 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | success x20 | 915 | 1017.4 / 1032.6 | 992.4-1038.4 | 0.00 | 1017.4 | 1005.0 / 1020.0 | 1003.9 / 1016.5 | 1133.3 / 1155.2 | 38 | 4 | 126.4 | 696 | 153.5 |
| mixed | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 730 | 784.6 / 796.0 | 772.9-798.4 | 0.00 | 784.5 | 782.0 / 793.0 | 806.4 / 818.6 | 943.5 / 950.5 | 38 | 8 | 126.4 | 696 | 153.5 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 545 | 593.0 / 601.0 | 587.8-601.9 | 0.07 | 592.9 | n/a | 618.4 / 622.3 | 753.9 / 765.3 | 38 | 22 | 126.4 | 696 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 730 | 782.4 / 797.1 | 768.9-798.7 | 0.23 | 782.3 | n/a | 964.9 / 975.1 | 1111.0 / 1118.0 | 38 | 8 | 126.4 | 696 | n/a |

## triage-large

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 0 | 47.5 / 51.4 | 42.1-53.4 | 0.00 | 47.4 | 39.0 / 42.0 | 41.8 / 45.1 | 166.2 / 181.6 | 34 | 4 | 375.2 | 696 | 399.5 |
| zero | codemode-c4 | 20/20 (cold 8/8) | success x20 | 0 | 42.8 / 49.0 | 36.4-49.3 | 0.00 | 42.8 | 35.0 / 40.0 | 42.4 / 45.5 | 168.0 / 170.6 | 34 | 3 | 375.2 | 696 | 399.5 |
| zero | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 0 | 20.0 / 21.1 | 17.2-26.3 | 0.00 | 20.0 | 18.0 / 19.0 | 42.3 / 43.9 | 167.6 / 169.2 | 34 | 7 | 375.2 | 696 | 399.5 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 0 | 25.9 / 29.6 | 23.7-29.7 | 0.07 | 25.8 | n/a | 53.2 / 54.2 | 179.2 / 181.2 | 34 | 1 | 375.2 | 696 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 0 | 12.7 / 16.4 | 11.0-18.5 | 0.12 | 12.6 | n/a | 197.0 / 199.1 | 325.8 / 327.5 | 34 | 8 | 375.2 | 696 | n/a |
| equal | codemode | 20/20 (cold 8/8) | success x20 | 210 | 283.9 / 298.3 | 273.3-298.5 | 0.00 | 283.9 | 273.0 / 282.0 | 273.6 / 281.5 | 409.4 / 420.1 | 34 | 8 | 375.2 | 696 | 399.8 |
| equal | codemode-c4 | 20/20 (cold 8/8) | success x20 | 330 | 414.8 / 424.7 | 400.4-425.1 | 0.00 | 414.8 | 402.0 / 410.0 | 409.0 / 415.0 | 549.5 / 556.0 | 34 | 4 | 375.2 | 696 | 399.8 |
| equal | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 210 | 258.9 / 268.8 | 252.9-279.9 | 0.00 | 258.8 | 255.0 / 264.0 | 275.7 / 284.1 | 411.7 / 419.2 | 34 | 8 | 375.2 | 696 | 399.7 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 120 | 158.3 / 162.8 | 154.7-167.2 | 0.07 | 158.2 | n/a | 182.0 / 183.1 | 315.9 / 320.9 | 34 | 22 | 375.2 | 696 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 210 | 246.0 / 252.7 | 238.9-255.5 | 0.24 | 245.7 | n/a | 425.2 / 429.3 | 567.7 / 575.3 | 34 | 8 | 375.2 | 696 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 490 | 567.1 / 575.4 | 553.1-585.2 | 0.00 | 567.1 | 555.0 / 561.0 | 559.1 / 576.4 | 703.2 / 712.4 | 34 | 8 | 375.2 | 696 | 399.8 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | success x20 | 675 | 768.3 / 777.9 | 755.7-780.2 | 0.00 | 768.2 | 755.0 / 764.0 | 760.1 / 769.9 | 900.8 / 917.8 | 34 | 4 | 375.2 | 696 | 399.8 |
| mixed | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 490 | 539.7 / 547.8 | 527.4-549.5 | 0.00 | 539.7 | 535.0 / 543.0 | 558.1 / 562.9 | 691.2 / 702.8 | 34 | 8 | 375.2 | 696 | 399.8 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 305 | 344.6 / 354.1 | 341.0-362.4 | 0.07 | 344.5 | n/a | 371.8 / 386.1 | 509.9 / 520.2 | 34 | 22 | 375.2 | 696 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 490 | 528.4 / 540.5 | 516.0-541.9 | 0.22 | 528.2 | n/a | 705.2 / 714.4 | 845.4 / 863.0 | 34 | 8 | 375.2 | 696 | n/a |

## denied

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | success x20 | 45 | 98.0 / 109.5 | 82.6-111.5 | 0.00 | 98.0 | 89.0 / 94.0 | 89.1 / 95.6 | 217.1 / 230.3 | 37 | 8 | 119.1 | 715 | 149.3 |
| zero | codemode-c4 | 20/20 (cold 8/8) | success x20 | 90 | 148.9 / 156.1 | 137.3-158.6 | 0.00 | 148.9 | 140.0 / 145.0 | 141.9 / 144.4 | 270.7 / 277.1 | 37 | 4 | 119.1 | 715 | 149.5 |
| zero | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 45 | 72.2 / 74.9 | 65.0-80.0 | 0.00 | 72.2 | 70.0 / 72.0 | 89.0 / 91.2 | 215.6 / 224.8 | 37 | 8 | 119.1 | 715 | 149.3 |
| zero | pi | 20/20 (cold 8/8) | success x20 | 15 | 41.8 / 49.0 | 37.8-56.7 | 0.07 | 41.7 | n/a | 64.9 / 67.2 | 193.5 / 200.7 | 37 | 22 | 119.1 | 715 | n/a |
| zero | opencode | 20/20 (cold 8/8) | success x20 | 45 | 65.0 / 70.5 | 63.5-77.9 | 0.12 | 64.9 | n/a | 250.0 / 256.7 | 379.1 / 381.9 | 37 | 8 | 119.1 | 715 | n/a |
| equal | codemode | 20/20 (cold 8/8) | success x20 | 375 | 466.9 / 477.1 | 441.5-485.0 | 0.00 | 466.9 | 454.0 / 465.0 | 456.3 / 461.4 | 592.7 / 600.9 | 37 | 8 | 119.1 | 715 | 149.7 |
| equal | codemode-c4 | 20/20 (cold 8/8) | success x20 | 525 | 622.9 / 640.5 | 606.8-641.3 | 0.00 | 622.9 | 612.0 / 628.0 | 622.3 / 633.7 | 759.7 / 771.9 | 37 | 4 | 119.1 | 715 | 149.7 |
| equal | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 375 | 436.4 / 444.9 | 426.6-445.3 | 0.00 | 436.4 | 434.0 / 442.0 | 458.8 / 474.7 | 596.2 / 600.5 | 37 | 8 | 119.1 | 715 | 149.7 |
| equal | pi | 20/20 (cold 8/8) | success x20 | 255 | 304.8 / 315.3 | 299.3-316.4 | 0.06 | 304.7 | n/a | 326.0 / 330.7 | 466.6 / 480.1 | 37 | 22 | 119.1 | 715 | n/a |
| equal | opencode | 20/20 (cold 8/8) | success x20 | 375 | 432.9 / 441.7 | 423.0-442.2 | 0.21 | 432.4 | n/a | 623.8 / 630.4 | 767.1 / 773.8 | 37 | 8 | 119.1 | 715 | n/a |
| mixed | codemode | 20/20 (cold 8/8) | success x20 | 765 | 859.8 / 877.4 | 844.1-889.5 | 0.00 | 859.8 | 850.0 / 860.0 | 856.3 / 874.2 | 994.7 / 1010.4 | 37 | 8 | 119.1 | 715 | 149.7 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | success x20 | 1010 | 1118.9 / 1133.9 | 1104.4-1151.9 | 0.00 | 1118.9 | 1109.0 / 1117.0 | 1115.2 / 1124.2 | 1254.0 / 1263.9 | 37 | 4 | 119.1 | 715 | 149.8 |
| mixed | codemode-prewarm | 20/20 (cold 8/8) | success x20 | 765 | 830.6 / 841.8 | 823.3-846.2 | 0.00 | 830.5 | 828.0 / 838.0 | 854.3 / 863.6 | 990.5 / 1001.5 | 37 | 8 | 119.1 | 715 | 149.7 |
| mixed | pi | 20/20 (cold 8/8) | success x20 | 560 | 609.0 / 628.1 | 601.2-629.1 | 0.06 | 608.9 | n/a | 634.0 / 637.9 | 771.1 / 784.6 | 37 | 22 | 119.1 | 715 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | success x20 | 765 | 829.3 / 836.6 | 820.6-837.6 | 0.24 | 828.8 | n/a | 1023.7 / 1032.9 | 1164.3 / 1178.3 | 37 | 8 | 119.1 | 715 | n/a |

## deadline

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 268.4 / 272.1 | 265.2-272.4 | 0.00 | 268.4 | 256.0 / 259.0 | 268.7 / 273.9 | 405.4 / 408.8 | 4 | 1 | 23.9 | 0 | 29.3 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 269.0 / 273.3 | 266.2-274.6 | 0.00 | 269.0 | 257.0 / 259.0 | 267.4 / 271.0 | 401.0 / 418.7 | 4 | 1 | 23.9 | 0 | 29.3 |
| mixed | codemode-prewarm | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 258.6 / 261.3 | 256.0-261.8 | 0.00 | 258.6 | 257.0 / 259.0 | 266.6 / 269.9 | 400.8 / 403.2 | 4 | 1 | 23.9 | 0 | 29.3 |
| mixed | pi | 20/20 (cold 8/8) | timeout (timeout) x20 | - | 252.6 / 259.2 | 252.3-264.0 | 0.07 | 252.4 | n/a | 262.8 / 265.0 | 400.3 / 410.1 | 4 | 1 | 23.9 | 0 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | timeout (TimeoutExceeded) x20 | - | 258.7 / 263.4 | 254.5-265.6 | 0.22 | 258.5 | n/a | 434.8 / 437.7 | 578.5 / 581.1 | 4 | 1 | 23.9 | 0 | n/a |

## guest-exception

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| zero | codemode | 20/20 (cold 8/8) | guest_error (guest_error) x20 | - | 44.5 / 48.7 | 35.1-51.7 | 0.00 | 44.5 | 36.0 / 38.0 | 36.4 / 38.7 | 162.2 / 175.0 | 28 | 8 | 125.4 | 0 | 145.5 |
| zero | codemode-c4 | 20/20 (cold 8/8) | guest_error (guest_error) x20 | - | 39.4 / 47.5 | 34.1-49.4 | 0.00 | 39.4 | 33.0 / 38.0 | 36.9 / 38.9 | 163.1 / 165.8 | 28 | 4 | 125.4 | 0 | 145.5 |
| zero | codemode-prewarm | 20/20 (cold 8/8) | guest_error (guest_error) x20 | - | 13.8 / 14.9 | 12.2-17.9 | 0.00 | 13.8 | 12.0 / 13.0 | 36.8 / 37.9 | 163.3 / 165.0 | 28 | 2 | 125.4 | 0 | 145.2 |
| zero | pi | 20/20 (cold 8/8) | guest_error (script) x20 | - | 23.3 / 25.3 | 18.7-25.3 | 0.06 | 23.2 | n/a | 48.1 / 50.1 | 177.8 / 180.1 | 28 | 1 | 125.4 | 0 | n/a |
| zero | opencode | 20/20 (cold 8/8) | guest_error (ExecutionFailure) x20 | - | 8.6 / 12.3 | 5.5-13.3 | 0.11 | 8.5 | n/a | 190.5 / 192.9 | 318.8 / 321.4 | 28 | 8 | 125.4 | 0 | n/a |

## lost-response

| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed | codemode | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 318.8 / 321.2 | 311.6-323.9 | 0.00 | 318.7 | 306.0 / 309.0 | 319.3 / 323.1 | 452.1 / 460.0 | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | codemode-c4 | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 317.9 / 324.6 | 310.6-330.8 | 0.00 | 317.9 | 306.0 / 309.0 | 317.9 / 322.7 | 453.6 / 458.2 | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | codemode-prewarm | 20/20 (cold 8/8) | timeout (deadline) x20 | - | 308.4 / 312.3 | 304.8-313.0 | 0.00 | 308.3 | 306.0 / 310.0 | 318.7 / 321.9 | 451.6 / 458.4 | 2 | 1 | 7.9 | 0 | 10.2 |
| mixed | pi | 20/20 (cold 8/8) | timeout (timeout) x20 | - | 302.5 / 306.4 | 301.0-310.9 | 0.06 | 302.4 | n/a | 314.2 / 315.1 | 452.1 / 464.2 | 2 | 1 | 7.9 | 0 | n/a |
| mixed | opencode | 20/20 (cold 8/8) | timeout (TimeoutExceeded) x20 | - | 307.9 / 313.8 | 303.2-314.4 | 0.18 | 307.4 | n/a | 477.1 / 482.2 | 622.8 / 626.8 | 2 | 1 | 7.9 | 0 | n/a |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| triage/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| triage-large/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/equal | opencode | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| denied/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| deadline/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | codemode-prewarm | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| guest-exception/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| lost-response/mixed | codemode | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-c4 | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | codemode-prewarm | none | 0 | 0 | 0 | 1 | none | - |
| lost-response/mixed | pi | none | 0 | 0 | 0 | 1 | none | Pi marks tracker.add_label cancelled; the ledger shows its effect applied (started+effect_applied+abort) (x20) |
| lost-response/mixed | opencode | none | 0 | 0 | 0 | 1 | none | OpenCode reports no outcome for tracker.add_label; the ledger shows its effect applied (started+effect_applied+abort) (x20) |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| triage/zero | codemode | trace_succeeded=38 x20 |
| triage/zero | codemode-c4 | trace_succeeded=38 x20 |
| triage/zero | codemode-prewarm | trace_succeeded=38 x20 |
| triage/zero | pi | pi_ok=38 x20 |
| triage/zero | opencode | admitted=38 x20; hook_started=38 x20 |
| triage/equal | codemode | trace_succeeded=38 x20 |
| triage/equal | codemode-c4 | trace_succeeded=38 x20 |
| triage/equal | codemode-prewarm | trace_succeeded=38 x20 |
| triage/equal | pi | pi_ok=38 x20 |
| triage/equal | opencode | admitted=38 x20; hook_started=38 x20 |
| triage/mixed | codemode | trace_succeeded=38 x20 |
| triage/mixed | codemode-c4 | trace_succeeded=38 x20 |
| triage/mixed | codemode-prewarm | trace_succeeded=38 x20 |
| triage/mixed | pi | pi_ok=38 x20 |
| triage/mixed | opencode | admitted=38 x20; hook_started=38 x20 |
| triage-large/zero | codemode | trace_succeeded=34 x20 |
| triage-large/zero | codemode-c4 | trace_succeeded=34 x20 |
| triage-large/zero | codemode-prewarm | trace_succeeded=34 x20 |
| triage-large/zero | pi | pi_ok=34 x20 |
| triage-large/zero | opencode | admitted=34 x20; hook_started=34 x20 |
| triage-large/equal | codemode | trace_succeeded=34 x20 |
| triage-large/equal | codemode-c4 | trace_succeeded=34 x20 |
| triage-large/equal | codemode-prewarm | trace_succeeded=34 x20 |
| triage-large/equal | pi | pi_ok=34 x20 |
| triage-large/equal | opencode | admitted=34 x20; hook_started=34 x20 |
| triage-large/mixed | codemode | trace_succeeded=34 x20 |
| triage-large/mixed | codemode-c4 | trace_succeeded=34 x20 |
| triage-large/mixed | codemode-prewarm | trace_succeeded=34 x20 |
| triage-large/mixed | pi | pi_ok=34 x20 |
| triage-large/mixed | opencode | admitted=34 x20; hook_started=34 x20 |
| denied/zero | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/zero | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/zero | codemode-prewarm | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/zero | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/zero | opencode | admitted=37 x20; hook_started=37 x20 |
| denied/equal | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/equal | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/equal | codemode-prewarm | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/equal | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/equal | opencode | admitted=37 x20; hook_started=37 x20 |
| denied/mixed | codemode | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/mixed | codemode-c4 | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/mixed | codemode-prewarm | trace_succeeded=35 x20; trace_provider_failed=2 x20 |
| denied/mixed | pi | pi_ok=35 x20; pi_error=2 x20 |
| denied/mixed | opencode | admitted=37 x20; hook_started=37 x20 |
| deadline/mixed | codemode | trace_succeeded=3 x20; trace_unknown=1 x20; unknown:started+abort=1 x20 |
| deadline/mixed | codemode-c4 | trace_succeeded=3 x20; trace_unknown=1 x20; unknown:started+abort=1 x20 |
| deadline/mixed | codemode-prewarm | trace_succeeded=3 x20; trace_unknown=1 x20; unknown:started+abort=1 x19; unknown:started+complete=1 x1 |
| deadline/mixed | pi | pi_ok=3 x20; pi_cancelled=1 x20; cancelled:started+abort=1 x20 |
| deadline/mixed | opencode | admitted=4 x20; hook_started=4 x20; interrupted:started+abort=1 x19 |
| guest-exception/zero | codemode | trace_succeeded=28 x20 |
| guest-exception/zero | codemode-c4 | trace_succeeded=28 x20 |
| guest-exception/zero | codemode-prewarm | trace_succeeded=28 x20 |
| guest-exception/zero | pi | pi_ok=28 x20 |
| guest-exception/zero | opencode | admitted=28 x20; hook_started=28 x20 |
| lost-response/mixed | codemode | trace_succeeded=1 x20; trace_unknown=1 x20; unknown:started+effect_applied+abort=1 x20 |
| lost-response/mixed | codemode-c4 | trace_succeeded=1 x20; trace_unknown=1 x20; unknown:started+effect_applied+abort=1 x20 |
| lost-response/mixed | codemode-prewarm | trace_succeeded=1 x20; trace_unknown=1 x20; unknown:started+effect_applied+abort=1 x20 |
| lost-response/mixed | pi | pi_ok=1 x20; pi_cancelled=1 x20; cancelled:started+effect_applied+abort=1 x20 |
| lost-response/mixed | opencode | admitted=2 x20; hook_started=2 x20; interrupted:started+effect_applied+abort=1 x20 |
