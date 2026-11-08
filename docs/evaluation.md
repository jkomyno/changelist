# Dispatch evaluation

The [engine benchmark](benchmark.md) compares codemode with Pi and OpenCode on a
realistic workflow and records what this work changed. This document covers the
earlier dispatch-policy study, measured at `d16dbc7`.

An offline, reproducible comparison of three dispatch policies on identical
fixtures. It measures one algorithmic question: when does refilling a free slot
(rolling) finish sooner than waiting for a whole batch, and what does it cost
when a run fails? It makes no claim about Rust being faster than another
implementation; every policy runs in the same Rust runtime and Deno guest.

## Run it

```sh
mise run eval    # about 100 s; writes eval/results/
```

`codemode-eval` ([source](../src/bin/codemode-eval.rs)) runs every fixture in
[`eval/fixtures.json`](../eval/fixtures.json) under each policy against the
offline mock provider. It prints a Markdown summary and, with `--out`, writes
`summary.json` (every run's metrics, aggregates, and a build fingerprint) and the
median run's full trace per fixture and policy. It exits non-zero if any check
fails. `tests/eval.rs` runs one repetition in `mise run check`.

## Method

| Policy | Dispatch | Cap |
| --- | --- | ---: |
| sequential | rolling with one slot | 1 |
| batch | start up to the cap when nothing runs; wait for the whole batch | 4 |
| rolling | start a queued call whenever a slot frees | 4 |

- **Comparable budgets.** Every policy gets the same timeout (5000 ms, or the
  fixture's override), call allowance (32), and output budget (256 KiB). Only the
  policy and cap differ; sequential uses a cap of 1 by definition.
- **Identical work.** Each fixture's guest submits all calls together through
  `Promise.all` or `Promise.allSettled`. Latencies come from `mock.wait`, so the
  provider work is deterministic.
- **Ordering.** 21 measured repetitions after 2 discarded warmups. Policies run
  round-robin within each repetition, and the starting policy rotates, so each
  policy starts first 7 times.
- **Metrics.** *Tool span*: earliest dispatch to the last observed result, from the
  trace; omitted when a run ends at its deadline or has unknown calls. *Elapsed*:
  the whole run, including about 50 ms of Deno startup. Percentiles use the
  nearest-rank method. Timestamps are whole milliseconds and sleeps overshoot by
  about 1 ms, so differences of 2 ms or less are not resolvable.
- **Model.** An ideal zero-overhead makespan computed from the fixture latencies:
  the sum for sequential, the sum of batch maxima for batch, and greedy list
  scheduling for rolling. It is a model, not a measurement.
- **Correctness.** Every run must match its expected status, output, and the
  outcome counts that timing cannot change. The harness also replays trace events to check each policy's behavior:
  rolling never leaves a slot idle while a call is queued, batch starts
  min(cap, queued) calls only when nothing runs and never refills mid-batch, every
  call has one admission and one terminal event that matches its record, and the
  tool span is never below the model. In a one-off check, swapping the two
  policies in the runtime made every fixture fail them.

## Results

Apple M1 Pro (10 logical CPUs), macOS, Deno 2.9.8, release build of commit
`d16dbc7`. The full output is in [`eval/results/summary.md`](../eval/results/summary.md);
traces are in [`eval/results/traces`](../eval/results/traces).

Tool span in ms (p50, with min–max):

| Fixture | Model seq / batch / rolling | Sequential | Batch | Rolling |
| --- | --- | ---: | ---: | ---: |
| mixed-latency: 2 × 200 ms + 6 × 25 ms | 550 / 400 / 225 | 570 (565–581) | 404 (402–410) | 230 (227–240) |
| equal-latency: 8 × 50 ms | 400 / 100 / 100 | 420 (415–434) | 104 (102–106) | 104 (103–109) |
| partial-failure: 6 calls, one fails at once | 330 / 240 / 120 | 342 (338–354) | 244 (243–248) | 122 (121–125) |
| fail-fast: 8 calls, the second fails at once | — | 245 (243–250) | 122 (121–126) | 122 (121–123) |

Elapsed p50, including startup: mixed-latency 635 / 465 / 294 ms; equal-latency
474 / 156 / 152 ms (sequential / batch / rolling).

Observed outcomes, identical in all 21 runs of each cell. These splits depend on
timing, so they are reported, not asserted:

| Fixture | Sequential | Batch | Rolling |
| --- | --- | --- | --- |
| fail-fast (`guest_error`) | 2 succeeded, 5 never dispatched | 3 succeeded, 4 never dispatched | 4 succeeded, 3 never dispatched |
| deadline (`deadline`, 540 ms) | 0 succeeded, 1 unknown, 11 never dispatched | 3 succeeded, 1 unknown, 8 never dispatched | 6 succeeded, 4 unknown, 2 never dispatched |

All 315 runs passed every check.

## What the measurements show

- **Mixed latency is where rolling helps.** With a slow call heading each batch,
  rolling finished the tool work in 230 ms against 404 ms for batch, close to the
  models (225 and 400 ms). Partial failure collected with `allSettled` shows the
  same pattern, 122 against 244 ms.
- **Equal latency removes the advantage.** Batch and rolling both took 104 ms; the
  difference is below what the timestamps resolve.
- **Startup matters at this scale.** Deno startup is about 50 ms per run, a third of
  the elapsed time for the equal-latency fixture.
- **Measured overhead is small but not zero.** The parallel policies ran 2–5 ms
  above the model and sequential 12–20 ms above it over 6–8 calls. That gap
  includes timer rounding and sleep overshoot, not only host work.
- **Rolling starts more work before a failure.** In fail-fast the host sees the
  failure before the guest does. Rolling and sequential dispatch refill the freed
  slot in the same step that forwards the failure, so one more call starts before
  the guest can report its error; batch waits for its whole batch. After the error,
  the host cancels queued calls but keeps collecting dispatched ones, so each run
  ends when its last dispatched call finishes: about 120 ms for batch and rolling,
  and about 245 ms for sequential, whose refill started a third 120 ms call.
  Rolling had started 4 calls that succeeded, against 3 for batch and 2 for
  sequential. For side-effecting tools, that is extra work after the program has
  already failed, not a gain.
- **Rolling completes more and leaves more uncertain at a deadline.** Rolling
  finished 6 calls before the deadline, against 3 for batch and none for
  sequential. It also left 4 dispatched calls `unknown`, against 1.

## Limits

- One machine, one Deno version, a mock provider, and fixtures of 6–12 calls. Real
  providers add variable latency, rate limits, and per-call process startup (1–2.5 s per
  call through the Composio CLI helper in the live check), which can dominate these effects.
- The fixtures submit all calls in one synchronous turn. Programs that interleave
  awaits and submissions may form smaller batches.
- The deadline split depends on startup. It holds for 0 to about 130 ms from run
  start to first dispatch (observed maximum 98 ms), so it is reported, not asserted.
  The fail-fast split assumes the guest reports its error before a running 120 ms
  call finishes; a slower report would let more calls start.

## Verification claims

| Claim | Kind | Evidence |
| --- | --- | --- |
| Policy timing, outcome splits, and invariants above | Offline | `mise run eval`, mock provider, 315 runs |
| Engine benchmark: correctness of codemode, Pi 1.4.0, and OpenCode 1.19.5 on six scenarios and a holdout, ledger reconciliation, latency, cold start, memory | Offline, synthetic tools | `mise run bench`, [`bench/results`](../bench/results), [benchmark](benchmark.md) |
| Trace schema 2: admission, dispatch, result, delivery, bytes, run-level events, host facts kept apart, bounded fact intake | Offline | `tests/runtime.rs`, `tests/bridge.rs` |
| Inspection: phases sum to elapsed time, evidence classes, reconciliation list, bounded output, no values or messages by default, control characters replaced, schema 1 traces | Offline | `tests/inspect.rs` |
| Prewarmed guests: deadline from hand-over, idle bridges stop on SIGTERM or guest exit | Offline | `tests/runtime.rs`, `tests/bridge.rs`, `tests/cli.rs` |
| Pi, OpenCode, and Composio adapters after protocol 2 and prewarming; OpenCode permission facts; Composio and Pi report only receipt | Offline, scripted models and fake helper | `mise run pi-check`, `mise run opencode-check`, `tests/composio.rs` |
| Rust runtime, bridge, and CLI contracts, including batch dispatch and SIGTERM | Offline | `mise run check` |
| Pi 1.4.0 and OpenCode 1.19.5 adapters unchanged | Offline, scripted models | `mise run pi-check`, `mise run opencode-check` |
| Composio result mapping, concurrent dispatch (a rendezvous that fails unless both identity reads overlap), unknown outcomes on cancellation and unobserved executions, credential absence, SDK executor | Offline, fake helper and stand-in SDK | `tests/composio.rs`, `mise run composio-demo` |
| Composio CLI 0.6.0: two read-only identity tools ran concurrently and succeeded, a CLI-rejected input became `composio_tool_failed`, an unauthorized slug was refused by Rust, and the CLI credential was absent from trace, summary, and stderr | **Live**, read-only, GitHub and Slack accounts | `mise run composio-live-check`, last run after the final adapter change |
| `@composio/core` against the live API | Not verified | Needs a project API key; only a CLI user key was available |
| Dispatch policies with a live provider | Not measured | The live check is a contract check, not a benchmark |
