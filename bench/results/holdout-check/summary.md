# Code-mode engine benchmark: holdout-check

Driver bun 1.4.4; bun 1.4.4, node v26.12.1, deno 2.9.8 (long term support, release, aarch64-apple-darwin). Apple M1 Pro (10 logical CPUs), darwin 14.4.1 arm64, 32 GiB. Commit 0e960b1d8e10 (bench/ uncommitted). Load average at start 3.47, 2.45, 2.1.

Warm: 0 discarded + 1 measured repetitions per cell (1 for failure scenarios), engines round-robin with a rotating start, in one bun process. Cold: 0 fresh bun processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. *Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.

## Limits as applied

| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |
| --- | --- | --- | --- | --- | --- |
| codemode | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| codemode-c4 | bridge protocol 1 (this checkout) | enforced by Rust; one absolute deadline that includes bridge and Deno startup | enforced by Rust; the first call beyond the allowance is rejected and ends the run | enforced by Rust's rolling scheduler (8, or 4 for codemode-c4) | enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each frame to the Deno guest is capped at 64 KiB |
| pi | @earendil-works/pi-codemode 1.4.0 (quickjs-wasi 3.6.2) | enforced by Pi (deadline armed before the worker starts) | not supported; not applied | not supported; every submitted call runs at once (unbounded) | not supported for tool replies; Pi caps text()/console output at 16 MiB characters only |
| opencode | @opencode-ai/codemode 1.19.5 (v1.19.5, 810c0eba1a73; effect 4.0.0-beta.83, acorn 8.15.0, typescript 5.8.2) | enforced by CodeMode limits.timeoutMs (cooperative, Effect.timeoutOrElse from execute()) | enforced by CodeMode limits.maxToolCalls | fixed at 8 by the engine (TOOL_CALL_CONCURRENCY); the requested value is not applied | not applied: CodeMode's maxOutputBytes truncates the final output and does not bound tool replies |

## release-audit (holdout: correctness only, timing not reported)

| Profile | Engine | Correct | Outcome | Calls started | Max in flight |
| --- | --- | ---: | --- | ---: | ---: |
| zero | codemode | 1/1 | success x1 | 37 | 5 |
| zero | codemode-c4 | 1/1 | success x1 | 37 | 3 |
| zero | pi | 1/1 | success x1 | 37 | 1 |
| zero | opencode | 1/1 | success x1 | 37 | 8 |
| mixed | codemode | 1/1 | success x1 | 37 | 8 |
| mixed | codemode-c4 | 1/1 | success x1 | 37 | 4 |
| mixed | pi | 1/1 | success x1 | 37 | 24 |
| mixed | opencode | 1/1 | success x1 | 37 | 8 |

## Correctness and ledger findings

Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).

| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| release-audit/zero | codemode | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/zero | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/zero | pi | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/zero | opencode | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/mixed | codemode | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/mixed | codemode-c4 | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/mixed | pi | none | 0 | 0 | 0 | 0 | none | - |
| release-audit/mixed | opencode | none | 0 | 0 | 0 | 0 | none | - |

## Engine-reported call outcomes (all measured runs)

| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |
| --- | --- | --- |
| release-audit/zero | codemode | trace_succeeded=37 x1 |
| release-audit/zero | codemode-c4 | trace_succeeded=37 x1 |
| release-audit/zero | pi | pi_ok=37 x1 |
| release-audit/zero | opencode | admitted=37 x1; hook_started=37 x1 |
| release-audit/mixed | codemode | trace_succeeded=37 x1 |
| release-audit/mixed | codemode-c4 | trace_succeeded=37 x1 |
| release-audit/mixed | pi | pi_ok=37 x1 |
| release-audit/mixed | opencode | admitted=37 x1; hook_started=37 x1 |
