# Native plans and embedded program execution

Approved plans submit operation identities directly to Rust's existing scheduler.
They need no JavaScript engine. Arbitrary programs can select an optional V8
embedding instead of the default external Deno executable. The embedded engine
still runs in a fresh child: deadlines, cancellation, providers, and the journal
remain under independent parent control.

## Build and select

```sh
mise exec -- cargo build --release --locked --features embedded-v8 --bin codemode
target/release/codemode run --backend embedded-v8 --code examples/compose.ts
target/release/codemode bridge --backend embedded-v8
mise run embedded-check
```

The default build supports Rust 1.85. The optional feature is tested on Rust
1.99.1; a complete minimum compiler version has not been established. Published
`deno_core 0.413.0` already uses syntax newer than 1.85. The locked embedding
dependencies are `deno_core 0.413.0`, `deno_ast 0.53.3`, and `deno_error 0.7.1`;
V8 comes through core's matching engine dependency. No `deno_runtime` is added.

Library consumers set `RunConfig::backend` to
`GuestBackend::EmbeddedV8 { executable }`, explicitly naming the worker binary.
Their own executable is not assumed to implement the worker entry point.
`PreparedGuest::start_embedded` supports starting it before source arrives.
The CLI selects its own executable. The shared host client accepts
`BridgeLimits.backend = "embedded-v8"`; the binary needs the feature too.
Selecting it in a default build produces an explicit error without falling back.

The embedding dependencies enable additional Serde JSON features. Plan and
approval hashes explicitly sort object keys, both builds use round-trip decimal
parsing, and dynamic JSON fields preserve literal keys instead of interpreting
Serde's private raw-value marker. Rust providers can use `codemode::parse_json`
or `codemode::deserialize_json` for the same literal-data behavior.
Previously normalized canonical journal values retain their identities. A raw
plan file whose decimal was parsed inaccurately by an earlier build can have a
corrected digest and require fresh approval; the old approval is never reused
for a changed value.
An offline cross-build check created an approved journal with the earlier
default binary containing both a decimal and a literal raw-value marker key,
then opened it with the final feature-enabled binary. The stored approval
remained readable; previewing the original inaccurately parsed decimal source
produced a different digest, as intended.

## Supported contract and limits

| Surface | Embedded worker |
| --- | --- |
| Source | JavaScript or TypeScript function body with `return`; types are transpiled, not checked |
| Helpers | `tools.call`, promises, `queueMicrotask`, `structuredClone`, and a console writing to discarded guest stderr |
| Values | JSON tool arguments/results; arguments cloned at call time; catchable provider errors retain `code` and `message` |
| Submission | Same-turn calls leave in one write; unawaited calls submitted before final output reach the parent |
| Built-ins | V8's language globals such as `Map`, `Set`, `Date`, and `JSON` |
| Unavailable | Deno/Node APIs, browser APIs such as `fetch`, `TextEncoder`, workers, and timers; filesystem, network, environment, subprocess, FFI, and module imports |
| Bounds | 64 KiB source and guest frames, 1 MiB host frames; the parent also enforces aggregate output, call, concurrency, and deadline limits |
| Lifetime | One process and fresh isolate per run; initialization may start early; no isolate reuse, snapshots, or pooling |

Only protocol operations are registered. There is no filesystem or network
module loader, and internal core imports are refused too. Every guest launches
with an empty environment. A native reader observes input closure independently
of JavaScript; a full reply burst cannot block it waiting on a bounded queue.
The parent still validates all guest stdout and can kill the child during a CPU
loop. These are runtime capability restrictions and process fault containment,
not an OS sandbox, a hard RSS limit, or proof that remote effects did not happen.

The optional worker is verified through library, CLI, and generic bridge paths.
The named Pi/OpenCode/Composio arbitrary-program adapters retain their existing
Deno verification; no live embedding claim is made for them. Native Pi and
Composio plan checks pass with a nonexistent inner Deno executable. The Composio
offline test's outer TypeScript host still uses Deno; the live CLI host uses
`composio run`.

## Measurement method

```sh
mise run bench-verify
mise run embedded-bench
mise exec bun@1.4.4 -- bun bench/run.ts \
  --engines codemode,codemode-prewarm,codemode-embedded,codemode-embedded-prewarm \
  --scenarios triage,triage-large --profiles mixed --memory --memory-samples 3 \
  --out bench/results/embedded-memory
```

All four configurations use the same built binary, provider fixtures, source,
concurrency of eight, call allowance, and output budget. The composed workflow
and its 148 KiB listing pages use the existing independent TypeScript/Python
expected outputs and call ledger. A no-op returns 42 with no tool calls.
Warm runs rotate engine order, discard two warmups, and record 15 repetitions;
cold samples use five fresh host processes per cell. Percentiles are nearest-rank.

Prewarming starts a new bridge and engine outside the next task's clock. The
existing harness observes the ledger for 100 ms after each result, also outside
that clock, allowing that preparation to progress. There is no ready handshake:
the report records elapsed prewarm time, not verified engine readiness. A cold
sample's first run has no prepared bridge. Task latency excludes the extra
observation and idle preparation; process and engine initialization are not free.

Memory is a separate pass using `ps` snapshots about 10 ms apart. It reports the
host, controller, guest, descendants, and the simultaneous full process tree.
Shared pages count in each process, and short peaks can fall between samples.
The post-result observation may include the next prewarmed worker. These are
sampled RSS measurements, not unique memory or enforced memory limits.

Native recovery uses the [matched recovery evaluation](recovery-evaluation.md):
every dispatching strategy uses `run_calls`, and guest startup is absent. Its
historical Deno measurements remain separate; changes in filesystem flush cost
prevent attributing the whole before/after difference to removing a guest.

## Results on this machine

Apple M1 Pro, 10 logical CPUs, 32 GiB, macOS 14.4.1, Rust 1.99.1 release,
Bun 1.4.4, Deno 2.9.8. The [full report](../bench/results/embedded/summary.md)
and [environment fingerprint](../bench/results/embedded/environment.json) record
the binary, compiler declaration, limits, and machine load. All 300 timed warm
runs and 100 cold samples passed their output and ledger checks, with zero
reconciliation violations. This is one machine, not an isolated performance lab;
the one-minute load average at timing start was 3.0.

Another [36 controlled-failure runs](../bench/results/embedded-failures/summary.md)
cover deadlines, guest exceptions, and a lost response after an applied effect
across all four configurations. All meet their expected failure and ledger
conditions with zero reconciliation violations; their durations are excluded
from the success-latency table. Cancellation is covered by the real-process
backend and host tests rather than timed here.

Warm task latency in milliseconds, **p50 / p95**:

| Workflow / provider latency | Deno | Embedded | Prewarmed Deno | Prewarmed embedded |
| --- | ---: | ---: | ---: | ---: |
| No-op / zero | 32.0 / 35.5 | 14.7 / 16.2 | 5.4 / 6.4 | 2.2 / 2.4 |
| Triage / zero | 37.8 / 52.6 | 18.9 / 22.2 | 12.6 / 14.9 | 8.0 / 9.3 |
| Large pages / zero | 38.0 / 40.3 | 20.6 / 21.5 | 15.8 / 16.4 | 10.4 / 11.0 |
| Triage / mixed | 799.1 / 805.2 | 773.5 / 781.7 | 768.5 / 776.9 | 761.2 / 766.7 |
| Large pages / mixed | 549.4 / 558.2 | 529.8 / 545.7 | 526.8 / 535.5 | 517.9 / 526.4 |

Embedding roughly halves the measured fresh-worker overhead on zero-latency
workflows. Provider latency dominates mixed cases: triage gains about 26 ms
without prewarming, and about 7 ms with it. Existing Deno prewarming is already
faster than starting a fresh embedded worker. Choosing the engine and choosing
when to initialize it are separate decisions. Cold samples show the first-run
cost too: triage's adapter-import-plus-task p50 was 39.1 ms with Deno and 23.0 ms
with embedding; the full fresh-host process wall times were 163.9 and 147.7 ms.

The separate [memory pass](../bench/results/embedded-memory/memory.md) has 24
correct runs, three per configuration and workflow. Median simultaneous whole
process-tree RSS was 84.3 MiB for fresh Deno triage and 67.7 MiB for fresh
embedded triage; large-page runs were 90.4 and 73.0 MiB. Both controller paths
used the same feature-enabled binary. These samples suggest a memory saving
for this workload; they do not establish a general bound.

The release binary grows from **6.2 MiB** (default features) to **75.2 MiB**
(`embedded-v8`), about 12.1 times larger. The first optional release dependency
build in this session took 86 seconds, with archives and debug artifacts already
cached; a default release build with its dependencies cached took 11 seconds.
These are observed builds, not a controlled clean-build comparison. The optional
feature also raises the compiler requirement and provides fewer globals than
Deno. Deno remains the default. No default change follows from this experiment.

Artifacts: [compressed warm runs](../bench/results/embedded/runs.jsonl.gz),
[cold samples](../bench/results/embedded/cold.jsonl), and
[binary/build observations](../bench/results/embedded/build-costs.json).
