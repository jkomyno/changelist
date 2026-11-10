# Code-mode engine benchmark

> The committed results under `results/` were measured at `8cb7a63`, `3f7e5ab`, and `6d26458`, as each `environment.json` records; codemode has changed since, and options or binaries named alongside them may no longer exist. Rerunning the harness measures the current code. See [Historical results](#historical-results) for the engine configurations they cover.

One set of host tools, datasets, and guest programs, run through matching code-mode engines
(`ENGINES` in `harness.ts`):

| Name | Engine | How it runs a program |
| --- | --- | --- |
| `codemode` | this repository, `target/release/codemode bridge` | Rust scheduler and trace; each run starts one bridge, which runs the program in a fresh embedded V8 worker process; host tools are called back over bridge protocol 2 through `integrations/shared/bridge-client.ts` |
| `codemode-c4` | same, concurrency 4 | secondary configuration at the bridge's usual cap |
| `pi` | `@earendil-works/pi-codemode` 1.4.0 | QuickJS-wasm VM in a fresh `node:worker_threads` worker per execute, in the driver process |
| `opencode` | `@opencode-ai/codemode` 1.19.5 | in-process tree-walking interpreter over a restricted JS subset (TS stripped, acorn parse) |

The goal is a fair, reproducible comparison with independently checked results. It does not
name a winner: every number holds only for its stated conditions (engine limits, latency
profile, process model, machine). Read [Semantic and limit differences](#semantic-and-limit-differences)
before comparing columns.

## Reproduce

Prerequisites: mise (Rust and Mr Boxington from the repository's `mise.toml`), Bun 1.4.4,
Node 26 (only for the Node spot-check), Python 3, curl, openssl, git. The default release
build includes the embedded V8 worker; no feature flag or separate guest runtime is needed.

```sh
mise run bench-setup   # bench/setup.sh: fetch + verify pinned engines into bench/.cache; build target/release/codemode
mise run bench-verify  # bench/verify-expected.sh: oracle.ts and check_expected.py must both agree with bench/expected.json

# Timing pass (about 15 minutes on an M1 Pro); `mise run bench` runs exactly this
mise exec bun@1.4.4 -- bun bench/run.ts --engines codemode,codemode-c4,pi,opencode \
  --scenarios triage,triage-large,denied,deadline,guest-exception,lost-response \
  --profiles zero,equal,mixed --reps 20 --warmup 3 --cold 8 --out bench/results/local

# Memory pass (separate; fresh process per sample)
mise exec bun@1.4.4 -- bun bench/run.ts --engines codemode,codemode-c4,pi,opencode \
  --scenarios triage,triage-large,denied,deadline,guest-exception,lost-response \
  --memory --memory-samples 5 --out bench/results/local

# Holdout correctness check (one untimed run per engine and profile)
mise exec bun@1.4.4 -- bun bench/run.ts --engines codemode,codemode-c4,pi,opencode \
  --scenarios release-audit --include-holdout --reps 1 --warmup 0 --cold 0 \
  --out bench/results/holdout-local

# Node 26 spot-check (codemode and Pi adapters only; OpenCode needs Bun)
node bench/run.ts --engines codemode,pi --scenarios triage --profiles zero,mixed \
  --reps 20 --warmup 3 --cold 8 --out bench/results/node-local
```

Use new output directories; the committed directories under `results/` are historical.

`CODEMODE_BIN` selects the actual executable for all configurations and is fingerprinted
by absolute path, SHA-256, and bytes. `CODEMODE_BUILD_RUSTC` and `CODEMODE_BUILD_FEATURES`
are optional operator declarations about that artifact; absent declarations remain null.
`toolchainRustc` records the currently selected compiler separately and does not prove which
compiler built an existing binary.

Other options: `--failure-reps N` (repetitions for failure scenarios; defaults to `--reps`).
The driver refuses OpenCode under Node. Holdout scenarios are skipped unless
`--include-holdout` is given.

## Layout

| Path | Role |
| --- | --- |
| `setup.sh` | fetch, verify, and install pinned upstream code into `.cache/`; build our release binary |
| `engines/pins.json` | every pin: versions, git tags and commits, npm tarball URLs and sha512 integrity |
| `engines/opencode-deps/` | `package.json` and `bun.lock` for OpenCode's three runtime dependencies |
| `engines/*.ts` | adapters, same contract (`engines/types.ts`) |
| `fixtures/datasets.ts` | seeded datasets A (issues) and B (pull requests, holdout) |
| `fixtures/tools.ts` | host tools, latency profiles, permission policy, simulated side effects |
| `fixtures/ledger.ts` | independent per-run ledger of tool operations |
| `workflows/*.js` | guest programs, written once in the subset all three engines accept |
| `scenarios.json` | program, dataset, tools, profiles, limits, expected outcome, holdout flag |
| `oracle.ts`, `check_expected.py`, `dump-datasets.ts`, `expected.json`, `verify-expected.sh` | expected outputs, computed twice independently |
| `model.ts` | ideal zero-overhead makespan for the triage-shaped scenarios |
| `harness.ts` | one run plus its correctness, ledger, and reconciliation checks |
| `run.ts`, `child.ts` | driver; fresh-process samples for cold and memory passes |
| `results/<label>/` | `runs.jsonl`, `cold.jsonl`, `summary.json`, `summary.md`, `environment.json`, and from `--memory` `memory.{jsonl,json,md}` |

Committed results are historical; see [Historical results](#historical-results). The
largest raw files are gzip-compressed (`runs.jsonl.gz`); `gunzip -k` restores them.

`.cache/` holds downloaded third-party code and must not be committed.

## Pins

From `engines/pins.json`, verified by `setup.sh`:

- **Pi**: `@earendil-works/pi-codemode@1.4.0` (git tag v1.4.0 = `4a080252f06caf63a525d7873c5d94d3c23887e0`)
  and `quickjs-wasi@3.6.2`, registry tarballs checked against their npm `sha512` integrity
  and unpacked into `.cache/pi/node_modules` (no install step, no scripts).
- **OpenCode**: `packages/codemode` of `anomalyco/opencode` at tag v1.19.5, which must resolve to
  `810c0eba1a7348036f8bdf61e6632d0c36c6876c` (shallow, blobless, sparse fetch; hooks disabled).
  Dependencies `acorn@8.15.0`, `effect@4.0.0-beta.83`, `typescript@5.8.2` installed with
  `bun install --frozen-lockfile --ignore-scripts` from the committed lockfile, which pins every
  transitive package with its integrity hash.
- **codemode**: this checkout, `mise exec -- cargo build --release --locked --bin codemode`.
  `environment.json` records the commit, a dirty flag (separately for paths outside `bench/`),
  and the selected binary's path, sha256, bytes, and declared build metadata.
- **Runtimes**: Bun 1.4.4 drives all engines; Node 26.12.1 for the spot-check. Each result
  directory records the versions it actually ran with.

## Adapter contract and shims

```ts
runEngine({ code, tools, limits: { timeoutMs, maxCalls, concurrency, maxOutputBytes }, signal? })
  -> Promise<{ ok, output?, error?: { kind, message, engineKind? }, engineTrace?, setupMs, runMs }>
```

`code` is a function body that calls `await tools.call(name, args)` and ends with `return`.
`tools[name](args, { signal?, report? })` returns a promise of JSON. `error.kind` is normalized
(`timeout`, `guest_error`, `call_limit`, `output_limit`, `cancelled`, `unsupported`,
`engine_error`, `harness_error`); `engineKind` keeps the engine's own label.

- **codemode**: no guest shim. Host shim: `execute(call, signal)` calls
  `tools[call.tool](call.arguments, { signal })`; a value becomes `{ok: true, value}`, a
  rejection `{ok: false, error: {code: error.code ?? "tool_error", message}}`. runBridge's third
  `report` argument is forwarded as `ctx.report`, so the fixtures' permission and executor facts
  reach codemode's trace (bridge protocol 2); other engines get no `report`. `setupMs` is near
  zero because process spawn and worker startup happen inside `runBridge`; the report's
  `elapsed_ms` (Rust clock) is shown separately. Each run starts its own bridge and worker.
- **pi**: Pi exposes `tools.<jsName>(args)` (dots become `_`) and has no `tools.call`. The
  program is wrapped on its first line so line numbers match:
  `const __piTools = tools, __piNames = {...}; return await (async (tools) => {<program>\n})({ call: async (name, args) => __piTools[__piNames[name]](args) });`.
  Each tool's `execute(args, { signal })` passes Pi's per-call signal as `ctx.signal`.
- **opencode**: CodeMode exposes `tools.<ns>.<tool>(input)`. The program is wrapped in a
  two-line prelude (diagnostic lines shift by +2) whose `call` resolves `name.split(".")` on
  the real tree. Each tool is `Tool.make({ input: { type: "object" }, run })` where `run` is
  `Effect.promise((signal) => fn(args, { signal }))` with OpenCode's own MCP failure projection
  (`toolError(message)`). `onToolCallStart`/`onToolCallEnd` hooks are recorded for the ledger
  comparison.

## Semantic and limit differences

Limits requested for success scenarios: `timeoutMs` 10000, `maxCalls` 64, `concurrency` 8,
`maxOutputBytes` 1048576. What each engine actually applies:

| | codemode | pi | opencode |
| --- | --- | --- | --- |
| Deadline | Rust, absolute; includes bridge spawn and worker startup | `timeoutMs`, armed before the worker starts | `limits.timeoutMs`, cooperative (`Effect.timeoutOrElse`) from `execute()` |
| Call allowance | enforced (64) | **none** | enforced (`maxToolCalls` 64) |
| Concurrency | rolling cap, 8 (`codemode-c4`: 4) | **unbounded**: every submitted call runs at once | **fixed at 8**, not configurable |
| Byte limits | shared reply/output budget (1 MiB here), a 1 MiB cap on each reply to the guest (64 KiB before `results/after`), and 64 KiB on each call frame from it | none for tool replies | `maxOutputBytes` truncates final output only; not passed |
| Process model | subprocesses (bridge + embedded V8 worker) per run | worker thread + wasm VM per run, in process | in process |
| Boundary encoding | JSON frames over stdio (host <-> Rust <-> V8 worker) | JSON strings across the VM boundary | `copyIn`/`copyOut` plain-data copies |
| Language | TS (transpiled) and JS on V8's language globals; no Deno or Node APIs | plain JS (TS annotations are syntax errors) | restricted subset (no classes, `new Promise`, `.then`, generators, timers) |

Behaviour verified in this benchmark's runs (see `results/baseline/summary.md`):

- **In-place sort**: OpenCode's `Array.prototype.sort(cmp)` (and `reverse`) returns a sorted
  copy and leaves the receiver unchanged. A program that sorts in place and then reads the array
  returns a wrong answer under OpenCode without any error. The programs therefore use sort's
  return value.
- **Property of undefined**: V8 (codemode) reports `Cannot read properties of undefined (reading 'name')`,
  Pi `TypeError: cannot read property 'name' of undefined`, OpenCode `Cannot access a property
  on a non-object value.` The guest-exception check accepts any of these.
- **Calls in flight at a deadline**: codemode records them `unknown`; Pi records them
  `cancelled`; OpenCode fires no end hook. In the lost-response scenario the ledger shows the
  label applied in every engine. Only codemode's record leaves the outcome open; Pi's
  `cancelled` does not mean the effect did not happen.
- **Abort on deadline**: all three engines abort the per-call AbortSignal of in-flight tools
  when the run ends (codemode via the host client when it reads the report).

## Fixtures

All data derives from mulberry32 streams and a fixed now, `2026-10-01T00:00:00Z`; timestamps
are second-precision ISO strings, so lexical order is time order.

- **Dataset A (seed 1)**: project `atlas`, 240 issues and 12 users in 4 teams (4 on call).
  30 issues carry `regression`; 22 of them are open with priority p0/p1; 2 of those (156, 158)
  are `confidential`. Details add a ~1.5 KiB body, 0-12 comments of ~200 characters (each at
  most 8 days before `updated_at`), and linked PRs.
- **Dataset B (seed 2)**: repo `orbit`, 150 pull requests (85% merged) with 3-5 CI checks each.
- **Tools**: `tracker.list_issues` (limit <= 40, 6 pages of ~8 KiB), `tracker.get_issue`,
  `directory.get_user`, `tracker.list_issues_large` (adds a 500-character `body_preview` and 6
  `history` entries; limit <= 200), `tracker.add_label` (simulated, in-memory; normal or
  lost-response), and for the holdout `vcs.list_pulls` (limit <= 50), `ci.get_checks`.
- **Latency profiles**, deterministic per call: `zero`; `equal` (30 ms each); `mixed` (list pages
  60 ms, `get_issue` 10/20/40/80/160 ms by FNV-1a of the id, `get_user` 25, `add_label` 40,
  `list_pulls` 50, `get_checks` 10/30/90 by FNV-1a of the PR number). Waits honour the call's
  AbortSignal.
- **Permission policy** (denial scenario only): `tracker.get_issue` waits 15 ms for "approval",
  then throws `code = "permission_denied"` for confidential issues.
- **Ledger**: the tool wrapper, not the engine, records `start`, `effect_applied`, and one
  terminal event (`complete`, `throw`, or `abort`) per operation, with canonical arguments and
  `t_ms` from the driver's start of the run. Result sizes are computed after the run, so
  serialization never adds to measured latency.

## Scenarios

| Scenario | Program | Profiles | Limits | Expected |
| --- | --- | --- | --- | --- |
| `noop` | `return 6 * 7`, no tool calls | zero | defaults | `42`, encoded independently in both oracles |
| `triage` | page 6x, filter, `Promise.all` 22 details, 10 owners, summarize (696 B) | zero, equal, mixed | defaults | output = `expected.json` |
| `triage-large` | same over `list_issues_large`, 2 pages of 120 items (151,771 and 152,060 B) | zero, equal, mixed | defaults | output = `expected.json` (codemode hit its former 64 KiB reply cap in `results/baseline`) |
| `denied` | triage with the policy; `Promise.allSettled`; returns `denied: [156, 158]` | zero, equal, mixed | defaults | output = `expected.json` |
| `deadline` | `triage.js` | mixed | timeoutMs 250 | `timeout` |
| `guest-exception` | triage, then reads `details[0].reviewer.name` | zero | defaults | `guest_error` |
| `lost-response` | one page, then `add_label` whose response never arrives | mixed | timeoutMs 300 | `timeout`; ledger: exactly 1 `effect_applied` without `complete` |
| `release-audit` (holdout) | page PRs, keep merged 2026-09-01..15, `get_checks` and authors in parallel | zero, mixed | defaults | output = `expected.json` |

Expected outputs are computed twice: `oracle.ts` reads the datasets in TypeScript with lexical
timestamp comparison; `check_expected.py` (stdlib only, run with `python3 -I`) reads a JSON dump,
parses timestamps as datetimes, and derives the cutoff from `now`. `verify-expected.sh` fails
unless both match the committed `expected.json`.

## Measurements

- **Warm**: one driver process, all engines loaded; per cell, `--warmup` discarded rounds then
  `--reps` measured rounds; within a round engines run round-robin and the starting engine
  rotates every round. *Task* is the driver clock around `runEngine` (setup + run). *Setup* and
  *Run* are the adapter's split. *Rust elapsed* is codemode's report `elapsed_ms`.
- **Cold**: `--cold N` fresh processes per engine and cell. *Cold total*: from just before the
  engine module's dynamic import to `runEngine`'s return (import + setup + run), measured inside
  the child after fixtures are loaded. *Cold process wall*: parent-observed spawn-to-exit,
  including runtime startup, fixture loading, and the 100 ms post-run observation window.
  Baselines: a bare `bun -e ""` and a fixture-only child.
- **Model**: ideal makespan from fixture latencies at the concurrency each engine applies
  (8, 4, unbounded for Pi, 8 for OpenCode), list-scheduled in submission order (`model.ts`).
  The no-tool `noop` model is 0 ms.
- **Statistics**: nearest-rank p50/p95, min, max, mean.
- **Bytes**: *payload* = canonical JSON bytes of all tool arguments plus all results the tools
  produced (from the ledger); *returned* = JSON bytes of the output handed back to the driver;
  *report* = JSON bytes of codemode's execution record. `engineBoundary` is reserved for codemode
  trace schema v2.
- **Memory** (`--memory`, never in the same process as timing runs): fresh process per sample,
  the slowest selected profile per scenario. Driver peak = `process.resourceUsage().maxRSS` minus
  a fixture-only baseline process, plus the same process's RSS as sampled by `ps` from the parent
  (Bun's `maxRSS` reads lower than `ps` on macOS). For codemode, `ps` samples distinguish the
  bridge controller directly under the driver and the embedded worker directly under a
  controller. Raw samples expose `psPeakBridgeKiB`, `psPeakWorkerKiB`, and `psPeakTreeKiB`;
  summaries expose `bridgePeakKiB`, `workerPeakKiB`, and `treePeakKiB`. Whole-tree peak is the
  largest simultaneous sum of driver plus all descendants in one snapshot, not a sum of
  independent process peaks. Sampling starts about every 10 ms; short-lived children and
  intervening peaks can be missed. RSS counts shared pages in full, so the total is not unique
  memory. Pi's worker threads and OpenCode's interpreter are inside the driver figure.
- **Correctness per run**: success scenarios deep-equal `expected.json` (canonical JSON);
  failure scenarios match the expected error kind (and message, for guest-exception) and any
  ledger expectation. A tool started more than 50 ms after the run returned also fails the run.
- **Ledger checks**: after each run the driver keeps observing for 100 ms, then waits (up to 1 s
  in total) for open operations to end. It reports late starts, tools still running after the
  run returned (completed, threw, or open; aborts after return are counted separately),
  `effect_applied` without `complete`, and the maximum number of operations in flight.
- **Reconciliation**: codemode's trace is matched to the ledger by (tool, canonical arguments)
  multisets: `succeeded`/`provider_failed` need a ledger `complete`/`throw`;
  `cancelled_before_dispatch`/`rejected` must have no ledger start; an `effect_applied` without
  `complete` must be `unknown`; for `unknown` the ledger state is reported. Pi's `result.calls`
  has no arguments, so its k-th record for a tool is matched to the k-th ledger op for that tool
  (Pi pushes the record and invokes the tool in the same step; matching by order is an
  inference from its source). OpenCode's hooks carry the input and are matched like codemode's
  trace; a start hook without an end hook is an interrupted call.
  The complete engine-reported trace, including its actual backend, remains in each warm
  raw run's `raw.engineTrace`.

## Caveats

- In-process engines (Pi's worker, OpenCode's interpreter) and the subprocess engine (codemode's
  bridge plus embedded V8 worker) pay different fixed costs; no-op and zero-latency
  cells mostly measure those costs.
- Pi runs every submitted call at once, so at `mixed` it can finish fan-outs faster than any
  capped engine; that reflects its lack of a concurrency limit, not a faster runtime. Compare
  against the *Model* column at each engine's own concurrency.
- OpenCode's concurrency cannot be lowered, so `codemode-c4` has no OpenCode counterpart.
- Under the `mixed` profile the 250 ms deadline fires while the program is still paging (six
  sequential 60 ms pages take 360 ms), not during the detail fan-out. The scenario therefore
  measures one in-flight list call at the deadline.
- `tracker.list_issues_large` allows 200 items per page, but the program requests 120: 200 gives
  pages of 247 KiB and 50 KiB, while 120 gives the two pages of roughly 120-160 KiB the scenario
  calls for.
- The `report` callback (`permission_*`, `executor_started`) reaches codemode's protocol 2
  trace. The independent fixture ledger and permission policy apply to every engine.
- Runtime (historical, Deno guest at `8cb7a63`): under Node 26.12.1 instead of Bun 1.4.4 (`results/node-spot-check`), warm triage
  p50 moved by +2.4 ms for codemode at `zero` (44.4 to 46.8) and +4.9 ms for Pi (25.1 to 30.0);
  at `mixed` both moved by under 1%. Fresh Node processes cost about 55-60 ms more wall time.
- Memory for scenarios that fail early (the deadline and exception scenarios, and codemode on
  `triage-large` in `results/baseline`) covers less work than a successful run and is not comparable across engines.
- One machine (Apple M1 Pro, macOS), synthetic tools, fixed latencies. Timer overshoot is about
  1 ms per wait. Real providers add variance, rate limits, and their own startup.
- Downloaded engine code runs in the driver process (Pi, OpenCode) with the driver's
  privileges; it is fetched from pinned, verified sources and installed without scripts, but it
  is not sandboxed.

## Historical results

Every directory under `results/` is a historical record, kept unchanged. Each
`environment.json` names its commit and binary. Several measure codemode engine
configurations that no longer exist: before `d9442b6`, `codemode` ran programs in a Deno
2.9.8 subprocess, and the harness also offered `codemode-prewarm` (one bridge and Deno guest
prepared before each run), `codemode-embedded` (an opt-in `embedded-v8` build, now the only
runtime), and `codemode-embedded-prewarm`. Their raw memory samples carry `psPeakDenoKiB` and
their summaries `denoPeakKiB`. Rerunning those engine names fails; the current harness has
only the engines listed at the top.

| Directory | Commit | Historical codemode configurations |
| --- | --- | --- |
| `baseline` | `8cb7a63`, before any optimization | `codemode`, `codemode-c4` on the Deno guest |
| `holdout-check`, `node-spot-check` | `8cb7a63` | `codemode` (and `codemode-c4` for holdout) on the Deno guest |
| `variants` | three builds interleaved; see its README | Deno guest |
| `after`, `holdout` | `3f7e5ab` | Deno guest, with `codemode-prewarm` |
| `embedded`, `embedded-memory`, `embedded-failures` | `6d26458` | `codemode`, `codemode-prewarm`, `codemode-embedded`, `codemode-embedded-prewarm` |

[docs/benchmark.md](../docs/benchmark.md) and
[docs/embedded-evaluation.md](../docs/embedded-evaluation.md) summarize them.
