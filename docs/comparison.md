# Comparison with other code-mode engines

This compares execution engines, not host adapters: how each runs a model-written
program that calls tools, what it limits, what it records, and what it reports
when a run fails. Every claim below was checked against pinned source and, where
stated, an actual run. Nothing here says the other engines lack a feature unless
it was looked for and not found in the files named.

## Pins

| Engine | Version | Source |
| --- | --- | --- |
| codemode | this repository | Rust runtime, Deno 2.9.8 guest |
| Pi | `@earendil-works/pi-codemode` 1.4.0 (the version Pi 1.4.0 ships) | tag `v1.4.0` = `4a080252f06caf63a525d7873c5d94d3c23887e0`; npm tarball integrity checked. `main` at `5ea81d2` has the same `src/runtime`. |
| OpenCode | `@opencode-ai/codemode` 1.19.5 | tag `v1.19.5` = `810c0eba1a7348036f8bdf61e6632d0c36c6876c`. `dev` at `a9a067f` renames the package (2.2.5) and adds a bytecode VM, suspend/resume, and durable tools that 1.19.5 does not have. |
| Cloudflare | `@cloudflare/codemode` 0.5.6 | `cloudflare/agents` at `be1749221ab3efc8be4e976d8e7d8344d1446032`; run locally with wrangler 4.158.0 (workerd 1.20261104.1) |

`bench/engines/pins.json` holds the pins the benchmark verifies. Paths below are
relative to each engine's package source at its pin.

## Execution engines

| | codemode | Pi 1.4.0 | OpenCode 1.19.5 | Cloudflare 0.5.6 |
| --- | --- | --- | --- | --- |
| Where the program runs | a Deno subprocess per run, in a permission-less worker | a QuickJS-wasm VM in a new `worker_threads` worker per run (`runtime/host.ts:20-75`), in the host process | a tree-walking interpreter in the host process and heap (`interpreter/runtime.ts:602`) | a new Dynamic Worker per run (`executor.ts:573-583`) |
| Language | TypeScript and JavaScript | JavaScript only; type annotations are syntax errors | a JavaScript subset with types stripped: no classes, `new Promise`, `.then`, timers, generators | JavaScript |
| Tool calls in flight | rolling scheduler with a host-set cap (default 4) | no cap: each call starts at once (`execution.ts:340`); 100 calls were in flight together | fixed cap of 8, rolling (`stdlib/promise.ts:6`); calls are counted only once they get a slot | no per-run cap found |
| Call allowance | host-set; the first excess call is recorded as rejected | none found | optional `maxToolCalls`, no default | none found |
| Deadline | host-set; the guest process is killed | wall clock, default 300 s (`execution.ts:49`); interrupt flag polled by the VM; stopped `while(true){}` at 502 ms | optional, cooperative (`Effect.timeoutOrElse`); a slow regex returned `ok: true` 79 ms past a 200 ms deadline | `Promise.race` against a sandbox timer, default 60 s; did not stop `while(true){}` locally and stalled a concurrent run |
| Memory | no limit (process isolation only) | optional heap limit; Pi's agent sets 256 MiB | none; shares the host heap | 128 MB per isolate (platform) |
| Result size | 1 MiB per reply and a shared byte budget | none for tool results; 16 Mi characters of output | optional output truncation after the run | none in the executor; 1 MB per recorded value in the durable runtime |
| Cancellation into tools | per-call signal through the provider; dispatched calls without a result are `unknown` | aborts every pending call's signal (`execution.ts:410-448`) | interrupts tool fibers; OpenCode's MCP calls get the session signal | none found |
| Per-call record | call identity, arguments, result or error, admission, dispatch, result and delivery times, bytes, provider facts | `{name, status, durationMs}` (`types.ts:53-57`) | `{name}`; optional start/end hooks with input, outcome, and `durationMs` | console logs only; the durable runtime logs each connector call with arguments and result |
| Approvals | the host's own gates, reported as facts | none in the engine; Pi's agent runs its hooks per nested call | none in the engine; OpenCode asks per nested call | `needsApproval` tools are dropped in the stateless path; the durable runtime pauses for approval |
| Replay or durability | none | deterministic `sandbox.start()` with a virtual clock and seeded random; durable replay in the separate `pi-durable` package | none at 1.19.5 (present on `dev`) | abort-and-replay on a Durable Object with divergence detection |
| Outcome of a call in flight at a deadline | `unknown` | `cancelled` | no end hook | — |

Three things the others do that codemode does not: Pi and Cloudflare can replay a
run deterministically or durably, Cloudflare's durable runtime pauses for
approvals and resumes, and Pi can cap guest heap memory. codemode has no
replay, durable state, or memory limit; its design defers them.

What codemode adds is a contract enforced outside the guest and evidence about
it. The deadline kills the guest process instead of relying on cooperation; the
call allowance, concurrency, and byte budget are host-set; and the record keeps
Rust's observations, the host's claims, and the uncertainty between them
separate, so a developer or agent can ask why a run was slow, which calls ran,
and which outcomes need checking (see [design](design.md#execution-evidence)).

## Measured on shared fixtures

The [benchmark](benchmark.md) runs one set of tools and programs through
codemode, Pi, and OpenCode under the same host (Bun 1.4.4), with outputs checked
against independently computed results and every tool call checked against a
ledger the tools keep themselves. Warm task time, p50 ms, after this work:

| Scenario | Latency | codemode | codemode, prewarmed | Pi | OpenCode |
| --- | --- | ---: | ---: | ---: | ---: |
| triage | zero | 40.7 | 14.0 | 23.0 | 13.5 |
| triage | equal (30 ms) | 408.7 | 385.4 | 284.9 | 380.0 |
| triage | mixed | 816.9 | 784.6 | 593.0 | 782.4 |
| triage-large (148 KiB pages) | zero | 47.4 | 20.0 | 25.9 | 12.7 |
| release-audit (holdout) | mixed | 473.6 | 443.1 | 300.6 | 437.7 |
| cold, fresh process | zero | 36.9 | 37.5 | 49.2 | 194.5 |

- Every engine produced the correct result in every run, including codemode on
  `triage-large`, which it could not complete before replies could exceed 64 KiB.
- With a prewarmed guest and the same concurrency cap, codemode is within 4
  percent of OpenCode on triage; Pi is faster under latency because it starts
  every call at once and applies no concurrency or call limit.
- At a deadline, codemode recorded the call in flight as `unknown`, Pi as
  `cancelled`, and OpenCode fired no end hook. In the lost-response case the
  ledger showed the effect applied in every run of every engine.
- Memory sits in different places: codemode adds about 3 MiB to the host and runs
  in two processes peaking at 45 MiB; Pi adds 55 MiB and OpenCode 83 MiB to the
  host process.

The benchmark doc lists every cell, the losses, and the method.

## Cloudflare, measured separately

Cloudflare's executor ran locally in workerd through Miniflare with a Worker
Loader binding, with tools forwarded to Node over one extra HTTP hop. These
numbers are not comparable with the table above: one workerd process serves all
runs, tools cross an extra hop, and production adds placement, isolate caching,
and platform CPU limits that were not exercised.

| Measurement | Local result |
| --- | --- |
| Cold setup (bundle and workerd start) | 87–236 ms |
| Warm no-op run | 3.6 ms median (2.8–4.2) |
| Two 50 ms tools with `Promise.all` | overlapped; 61 ms |
| Eight calls | peak 8 in flight; 60 ms |
| ~200 KiB tool result | delivered intact |
| `while(true){}` with a 500 ms timeout | not stopped; workerd was killed by the harness at 3.5 s |
| Guest `fetch` | refused (`globalOutbound: null`) |

## Limits of this comparison

- One machine (Apple M1 Pro, macOS 14.4), Bun 1.4.4 as the host for every engine,
  synthetic tools with fixed latencies, and five workflows. Real providers add
  variable latency, rate limits, and network failures.
- The engines differ in what they enforce. A faster engine without a call cap,
  a hard deadline, or a byte budget is not doing the same work; the benchmark
  states each engine's applied limits.
- Pi and OpenCode run inside the host process; codemode starts two subprocesses
  per run. Prewarming moves that startup off the run's critical path; it does not
  remove its cost.
