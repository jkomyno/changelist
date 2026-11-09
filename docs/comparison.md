# Comparison with other code-mode implementations

This page has two parts. The current comparison runs one GitHub triage workflow through
codemode, Pi Durable, OpenCode Code Mode, and a competent Composio-native script against one
offline provider with an independent ledger. The historical comparison below it compared
execution engines on shared fixtures at earlier pins; it is kept unchanged as a record.

## Current comparison (2026-11-10)

Every claim in this section cites a file in
[`bench/workflow/results/2026-11-10/`](../bench/workflow/results/2026-11-10/):
`summary.md` and `summary.json` for the tables, `runs.jsonl` for each run, `environment.json`
for the machine and pins. The method, the advantage hypotheses, and the scoring rules were
written in [`bench/workflow/README.md`](../bench/workflow/README.md) before the first run.

### What was compared

| Implementation | Measured | Released or dev | Source pin |
| --- | --- | --- | --- |
| codemode | this repository at `077dcab` (`target/release/codemode`, Rust 1.99.1) | unreleased | the commit itself |
| Pi Durable | `@earendil-works/pi-durable` 1.5.0 (npm, 2026-11-02, marked Experimental), with `pi-codemode` 1.5.0 | released | tag `v1.5.0` = `f9a2636`; `main` at `77959d6` (2026-11-09) is ahead by printing fixes only and was not measured |
| OpenCode Code Mode | `@opencode/codemode` 2.2.6 (npm, 2026-11-06), released interpreter | released | tag `v2.2.6` = `dc2ffcf`; `dev` at `9cb8e65` (2026-11-09) documents `Tool.idempotent` and `Tool.durable`, which 2.2.6 lacks; not measured |
| Composio script | [`engines/composio-script.ts`](../bench/workflow/engines/composio-script.ts), written for this comparison | n/a | published with the results |
| Cloudflare Code Mode | `@cloudflare/codemode` 0.5.6, `cloudflare/agents` `5c33d89` (2026-11-06) | released | documented, not run: it needs workerd and a Durable Object facet |

Tarball integrity for every package is in
[`bench/workflow/pins.json`](../bench/workflow/pins.json) and is checked at install.

### The workflow and the scoring

The offline repository has seven issues; the triage workflow derives eleven writes from them:
three label additions, three label removals, three marked comments, and two closes. All four
implementations make the same decision from the same reads. A separate provider process
serves Composio-shaped GitHub tools with 20 ms latency and no idempotency keys, and writes an
fsynced ledger that outlives every crash. Each run's final state and each implementation's
claims are scored against that ledger; the flags and verdicts are defined in the
[README](../bench/workflow/README.md#scenarios-and-oracles). Each fault cell ran 5 times and
each latency cell 15 times, in two rows: `host` (each implementation's own defaults) and
`equal` (the same 10 s deadline, 64 MiB heap, 64 calls, and concurrency 1 everywhere).

### Hypotheses and what the runs showed

| Hypothesis (fixed before the first run) | Result | Evidence in `summary.md` |
| --- | --- | --- |
| H1a. Against the script: approval bound to the reviewed digest, account, and destinations | **Demonstrated.** A change list widened after review was refused with the reviewed digest (exit 2, "does not match the plan's digest") and its extra write never reached the provider; the script ran it. After the account switched to another GitHub login, codemode refused every session before dispatch (0 of 10 runs sent a write); the script sent writes as the other login in 10 of 10. | Properties; `account-switched` |
| H1b. Against the script: contract-based reconciliation of a lost comment reply | **Demonstrated on safety, tied elsewhere.** When the marked comment landed after recovery began (`late-effect`, `probe-budget`), the script posted it again in 20 of 20 runs; codemode never did. Where the marker was already there (`lost-response`, `broken-transport`, `slow-reply`) both settled without a duplicate. | Hypotheses section, head-to-head |
| H2. Against Pi Durable: exact approval binding | **Demonstrated.** Pi ran the widened change list, sent writes after the account was revoked (10 of 10, denied by the provider) or switched (10 of 10, applied as the other login), and wrote to an issue a maintainer had just closed (10 of 10). codemode sent none of these. | Properties; `account-*`, `scope-changed` |
| H2. Against Pi Durable: contract-only reconciliation | **Demonstrated as settlement; tied on duplicate safety.** After a lost reply, a held comment, a broken connection, or a kill after the first write (equal row), codemode settled the interrupted write as applied or satisfied from provider evidence in every run; Pi reported it "interrupted and may have partially run" in every run and left it to a person. Neither duplicated anything. Pi's restart limit ended `repeated-crash` after 5 restarts with work undone; codemode completed it. | `lost-response`, `late-effect`, `broken-transport`, `repeated-crash` |
| H3. Against OpenCode Code Mode: any durable recovery | **Demonstrated as a capability: OpenCode has none.** Rerunning its convergent program from scratch duplicated the late comment in 20 of 20 `late-effect` and `probe-budget` runs; codemode never did. The rerun did finish more often in plain kill cases, below. | `late-effect`, `probe-budget` |
| H0. Latency: codemode slower than all three on the no-fault workflow | **Held in 5 of 6 comparisons.** codemode was faster than the script in the host row, because it dispatches four writes at a time. | Latency |

### Per-scenario outcomes

Counts of runs per verdict: `correct` (final state as expected, nothing claimed or sent
wrongly), `review` (as expected, but the implementation reports an uncertain operation and needs
a person), `incomplete` (not finished, nothing claimed or sent wrongly), `wrong` (a duplicate,
an unsafe send, or an unjustified claim). OpenCode's crash cells are reruns from scratch.

| Scenario | Row | codemode | Pi Durable | OpenCode (rerun) | Composio script |
| --- | --- | --- | --- | --- | --- |
| `lost-response` | host | incomplete 4 correct 1 | review 5 | correct 5 | correct 5 |
| `lost-response` | equal | correct 5 | review 5 | correct 5 | correct 5 |
| `kill-before-first-write` | host | incomplete 5 | incomplete 5 | correct 5 | correct 5 |
| `kill-before-first-write` | equal | incomplete 5 | incomplete 5 | correct 5 | correct 5 |
| `kill-after-first-write` | host | incomplete 5 | review 5 | correct 5 | correct 5 |
| `kill-after-first-write` | equal | correct 5 | review 5 | correct 5 | correct 5 |
| `late-effect` | host | incomplete 4 correct 1 | review 5 | wrong 5 (duplicate 5) | wrong 5 (duplicate 5) |
| `late-effect` | equal | correct 5 | review 5 | wrong 5 (duplicate 5) | wrong 5 (duplicate 5) |
| `scope-changed` | host | review 5 | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) | correct 5 |
| `scope-changed` | equal | review 5 | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) | correct 5 |
| `account-revoked` | host | incomplete 5 | wrong 5 (unsafe send 5) | incomplete 5 | incomplete 5 |
| `account-revoked` | equal | incomplete 5 | wrong 5 (unsafe send 5) | incomplete 5 | incomplete 5 |
| `account-switched` | host | incomplete 5 | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) |
| `account-switched` | equal | incomplete 5 | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) | wrong 5 (unsafe send 5) |
| `ignore-cancel` | host | correct 5 | correct 5 | correct 5 | correct 5 |
| `ignore-cancel` | equal | correct 5 | incomplete 5 | correct 5 | correct 5 |
| `slow-reply` | host | correct 5 | correct 5 | correct 5 | correct 5 |
| `slow-reply` | equal | correct 5 | incomplete 5 | incomplete 5 | correct 5 |
| `broken-transport` | host | correct 5 | review 5 | review 5 | correct 5 |
| `broken-transport` | equal | correct 5 | review 5 | review 5 | correct 5 |
| `repeated-crash` | host | correct 5 | incomplete 5 | correct 5 | correct 5 |
| `repeated-crash` | equal | correct 5 | incomplete 5 | correct 5 | correct 5 |
| `probe-budget` | host | incomplete 4 review 1 | review 5 | wrong 5 (duplicate 5) | wrong 5 (duplicate 5) |
| `probe-budget` | equal | review 5 | review 5 | wrong 5 (duplicate 5) | wrong 5 (duplicate 5) |

In the account scenarios, `incomplete` is the best possible verdict. No implementation made an
unjustified success or non-effect claim in any run; the oracle flags them, and its own checks
(`oracle.test.ts`) show it would.

What the table says about each:

- **codemode** never duplicated a comment, never sent an unapproved or out-of-scope write, and
  never claimed an effect it could not show. Its losses are completion: an operation whose
  intent it journaled but which never reached GitHub stays `unknown` until a person reviews it,
  because a missing label or comment is not proof the request will not still land. With four
  writes in flight (host row), a kill strands up to three siblings that were still in their
  precondition reads; that is why `lost-response`, `late-effect`, `kill-after-first-write`, and
  `probe-budget` end incomplete in the host row but correct (or, for `probe-budget`, at review
  after its 3 probes) with concurrency 1. `kill-before-first-write` is a loss in both rows: the
  first write never left the host, and codemode will not send a label write again without proof
  (the KTD9 contract is `repeat: unsafe`).
- **Pi Durable** never duplicated a write either: it does not rerun an interrupted write. It
  reports that write as interrupted and moves on, so every crash cell needs a person, and it
  has no account, scope, or approval check, so it sent writes after a revocation, after an
  account switch, and to a closed issue. Its deadline fails the whole program (`ignore-cancel`,
  `slow-reply`, equal row), reporting the in-flight call as `cancelled`; resuming the settled run
  does nothing. Its restart limit (5) ended `repeated-crash` with work undone.
- **OpenCode Code Mode** has no recovery; a rerun of the convergent program finished every plain
  kill case, and duplicated the comment whenever the first one landed late. It sent writes as
  another login after the account switch and to the closed issue. At its deadline it reports the
  run as timed out without naming the call in flight.
- **The Composio script** is the strongest completion baseline: it finished every kill case and
  refused to write to the closed issue. It duplicated the comment whenever its marked comment
  was absent at recovery and landed later (20 of 20), and it kept writing as another login after
  the account switch.

### Cancellation and slow replies

In the equal row (1.5 s deadline; the provider applies the label 3 s after a cancelled request),
codemode recorded the cut-off call as `unknown`, then `satisfied` once the late effect landed;
the script recorded `unknown` and then found the label. Pi reported the nested call as
`cancelled` and failed the run; OpenCode reported a timeout without naming the call. With host
defaults nothing set a deadline below 3 s, so every implementation waited and finished. Source:
`summary.md`, the "first reported claim" column of `ignore-cancel` and `slow-reply`.

### Latency, no-fault workflow

Process start to exit of the last attempt, in milliseconds, 15 timed runs per cell. codemode's
time includes its investigation bridge, `plan check`, `plan approve`, and `plan run`; Pi's
includes its faux model turns.

| Row | Implementation | p25 | p50 | p75 | p95 | Startup p50 | Peak RSS MiB (sampled) | Durable records |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| host | codemode | 861 | 868 | 873 | 895 | 98 | 152 | 25 journal facts |
| host | Pi Durable | 812 | 814 | 821 | 834 | 230 | 191 | 104 storage rows |
| host | OpenCode | 767 | 774 | 777 | 782 | 249 | 174 | 0 |
| host | Composio script | 1218 | 1222 | 1227 | 1237 | 94 | 121 | 23 progress lines |
| equal | codemode | 1308 | 1315 | 1322 | 1337 | 98 | 152 | 25 journal facts |
| equal | Pi Durable | 802 | 813 | 817 | 819 | 230 | 191 | 104 storage rows |
| equal | OpenCode | 766 | 771 | 778 | 783 | 250 | 168 | 0 |
| equal | Composio script | 1218 | 1221 | 1227 | 1231 | 94 | 120 | 23 progress lines |

By the tie rule (p50 within 10 percent and overlapping p25–p75), no cell tied. codemode lost to
Pi and OpenCode in both rows and to the script in the equal row, and beat the script in the host
row. The script and codemode both read issue state before each write, which is most of their
extra time; startup is the time to the provider's first request. Against live Composio calls,
which take seconds each, these differences are small.

### Runaway programs

| Program | Row | codemode | Pi Durable | OpenCode |
| --- | --- | --- | --- | --- |
| CPU loop | host | stopped by its 5 s default deadline | not stopped; harness kill at 20 s | not stopped; harness kill at 20 s |
| CPU loop | equal (3 s) | stopped at 3.1 s | stopped at 3.2 s | stopped at 3.2 s |
| Memory growth | host | `memory_limit` at 0.8 s (256 MiB heap default), peak RSS 361 MiB | not stopped; 1.3 GiB at the 20 s kill | not stopped; 3.5 GiB at the 20 s kill |
| Memory growth | equal (64 MiB) | `memory_limit` at 0.24 s | out of memory at 0.42 s | host process killed (`SIGABRT`) at 0.78 s |

The script has no guest program, so these cells are unsupported for it.

### Properties

| Property | codemode | Pi Durable | OpenCode | Composio script |
| --- | --- | --- | --- | --- |
| Edited or widened change list runs without re-approval | no | yes (no approval step) | yes (no approval step) | yes (no approval step) |
| Progress inspectable from another process during a run | yes, `plan inspect --json` (10 of 10) | only by opening its SQLite storage read-only; its `inspect()` and `watch()` are in-process | no | yes, its progress file |
| Exportable diagnosis after an interrupt naming each operation applied, not applied, or unknown | yes, `plan inspect --json` names all eleven (10 of 10) | no supported export | no | yes, its progress file names all eleven |
| Install | build from source (75 MiB binary) | 93 npm packages, 117 MiB, plus a host program | 3 npm packages, 63 MiB, plus a runner | none beyond Node and the Composio CLI |

The script's progress file matches codemode's inspection and diagnosis because R28 requires the
script to keep one; that cell is a tie.

### Mismatches between the implementations

These differences are part of what is measured; each is stated so the cells are read correctly.

- **What is recovered differs.** codemode executes a finite approved plan and never replays the
  program. Pi Durable replays the program against its journal and does not rerun an interrupted
  write. OpenCode keeps nothing, so the driver reran the program from scratch; that it converges
  is the program's property, not the engine's. The script resumes its own decision list.
- **Approval exists only in codemode.** Pi, OpenCode, and the script have no step that binds
  a reviewed change list; "runs without re-approval" is the absence of that step, not a defect
  in their engines.
- **Reads before writes.** codemode's plan host and the script read issue state before each
  write; the Pi and OpenCode programs read once and then write, as a typical code-mode program
  does. `scope-changed` follows from that.
- **Concurrency.** codemode dispatches four operations at a time by default; the three programs
  await one write at a time. The equal-limits row sets codemode to one. Several codemode
  host-default losses come from operations whose intent was journaled while a sibling's
  precondition read was in flight: after the kill they were never sent, and codemode will not
  send them again without proof.
- **The add-labels contract is `repeat: unsafe` (KTD9).** codemode will not re-send a label
  write whose intent it recorded, even when the label is still missing; the script and the
  OpenCode rerun read the issue and send it. That is why `kill-before-first-write` ends in
  review for codemode and complete for them.
- **Uncertainty wording.** codemode and the script have their own `unknown` state. For Pi and
  OpenCode, the benchmark's tool wrapper (the same for both) reports a transport failure as
  "outcome unknown"; without it, those cells would show `failed` claims for applied comments.
  Pi's own "interrupted and may have partially run" is counted as unknown.
- **Pi's deadline and recovery.** A durable Pi run accepts a deadline only through a first-line
  `// @options` comment; at the deadline the whole program fails and the run settles. The
  driver's rerun waits on the same submission, which Pi has already settled; a new submission
  would rerun the program from scratch and was not tried.
- **Memory limits.** OpenCode has no memory limit; its equal-limits heap is
  `node --max-old-space-size` on its own process, so reaching it kills the host (`SIGABRT`).
  codemode's limit is the worker's V8 heap; Pi's is QuickJS `memoryLimitBytes`.
- **Pi needs a model turn.** The pi-ai faux provider answers with the program; its turns are
  charged to Pi's latency.
- **codemode's host is the benchmark's.** The plan host in
  `bench/workflow/engines/codemode.ts` implements the KTD9 contracts against the offline provider;
  it is not the installed Composio host, whose rewrite is separate work.

### Limits of this evidence

- One machine (Apple M1 Pro, 10 CPUs, macOS 14.4.1), Node 24.22.1, one run of the protocol.
  Load averages before each batch are in `batches.json`.
- An offline provider with a fixed 20 ms latency. Live Composio calls take seconds each, so the
  latency differences above (tens to hundreds of milliseconds) would mostly disappear; the
  correctness results do not depend on latency.
- Five runs per fault cell. Most outcomes were identical across runs; where they were not, the
  cell shows the split. The kill points are provider events, so which sibling operations are in
  flight at a kill varies with timing.
- Peak RSS is sampled every 200 ms with `ps`, per process group; it is not a ceiling.
- Nobody performed a human review. Runs that end with operations awaiting review are counted as
  such, not completed on a person's behalf.
- Cloudflare Code Mode was not run. Pi `main` and OpenCode `dev`, which are ahead of the measured
  releases, were not run.
- Live Composio behavior (account resolution, CLI temporary files, real GitHub timing) is outside
  this comparison.

## Historical comparison (2026-11-08, pins codemode `3f7e5ab`, Pi 1.4.0, OpenCode 1.19.5, Cloudflare 0.5.6)

Kept as recorded on 2026-11-08; only its heading levels changed to nest under this heading.

> Historical record: codemode's cells describe the engine pinned and measured at `3f7e5ab` (the [benchmark](benchmark.md)'s `after` results). codemode has changed since; options and binaries it names may no longer exist.

This compares execution engines, not host adapters: how each runs a model-written
program that calls tools, what it limits, what it records, and what it reports
when a run fails. Every claim below was checked against pinned source and, where
stated, an actual run. Nothing here says the other engines lack a feature unless
it was looked for and not found in the files named.

### Pins

| Engine | Version | Source |
| --- | --- | --- |
| codemode | this repository | Rust runtime, Deno 2.9.8 guest |
| Pi | `@earendil-works/pi-codemode` 1.4.0 (the version Pi 1.4.0 ships) | tag `v1.4.0` = `4a080252f06caf63a525d7873c5d94d3c23887e0`; npm tarball integrity checked. `main` at `5ea81d2` has the same `src/runtime`. |
| OpenCode | `@opencode-ai/codemode` 1.19.5 | tag `v1.19.5` = `810c0eba1a7348036f8bdf61e6632d0c36c6876c`. `dev` at `a9a067f` renames the package (2.2.5) and adds a bytecode VM, suspend/resume, and durable tools that 1.19.5 does not have. |
| Cloudflare | `@cloudflare/codemode` 0.5.6 | `cloudflare/agents` at `be1749221ab3efc8be4e976d8e7d8344d1446032`; run locally with wrangler 4.158.0 (workerd 1.20261104.1) |

`bench/engines/pins.json` holds the pins the benchmark verifies. Paths below are
relative to each engine's package source at its pin.

### Execution engines

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

### Measured on shared fixtures

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

### Cloudflare, measured separately

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

### Limits of this comparison

- One machine (Apple M1 Pro, macOS 14.4), Bun 1.4.4 as the host for every engine,
  synthetic tools with fixed latencies, and five workflows. Real providers add
  variable latency, rate limits, and network failures.
- The engines differ in what they enforce. A faster engine without a call cap,
  a hard deadline, or a byte budget is not doing the same work; the benchmark
  states each engine's applied limits.
- Pi and OpenCode run inside the host process; codemode starts two subprocesses
  per run. Prewarming moves that startup off the run's critical path; it does not
  remove its cost.
