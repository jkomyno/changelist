# Engine benchmark

> Historical record, measured at `8cb7a63` (baseline) and `3f7e5ab` (after and holdout). codemode has changed since; options and binaries it names may no longer exist, and rerunning its commands measures the current code, not those commits.

One set of host tools, seeded datasets, and guest programs, run through
codemode, Pi's codemode engine 1.4.0, and OpenCode's codemode engine 1.19.5,
with each engine's results checked against expected outputs computed
independently and against a ledger of what every tool call actually did. It
measures where codemode's time goes, what three changes bought, and how the
engines compare under stated limits. [The comparison](comparison.md) covers
what each engine is; [`bench/README.md`](../bench/README.md) covers the harness.

## Run it

```sh
mise run bench-setup    # fetch and verify pinned engines; build the release binary
mise run bench-verify   # expected outputs, computed in TypeScript and in Python
mise run bench          # about 15 minutes; writes bench/results/local
mise run demo           # one workflow, prewarming, a denial, a lost response, inspection
```

## Method

- **Fixtures first.** The scenarios, datasets, latencies, and expected outputs
  were committed in `8cb7a63`, before any optimization. A holdout workflow
  (`release-audit`, over a second dataset) was checked for correctness then and
  first timed after the last change.
- **Scenarios.** `triage`: six dependent page requests, parallel detail lookups
  for about two dozen issues, parallel owner lookups, filtering, and a 696-byte
  answer (38 calls, 126 KiB of tool results). `triage-large`: the same over
  148 KiB pages (375 KiB of results). `denied`: a host permission gate refuses
  two lookups. `deadline`, `guest-exception`, and `lost-response` (an effect
  applied whose response never arrives) are failure cases with no real side
  effects.
- **Latency.** `zero`; `equal` (30 ms per call); `mixed` (10–160 ms by call).
- **Limits.** 10 s, 64 calls, concurrency 8, 1 MiB output budget, as each engine
  supports them. Pi applies no call or concurrency limit; OpenCode's concurrency
  is fixed at 8. `codemode-c4` runs at codemode's default cap of 4.
- **Engines.** All run with Bun 1.4.4 as the host. Pi and OpenCode run inside the
  host process; codemode starts a bridge and a Deno guest per run.
  `codemode-prewarm` keeps one bridge and guest started ahead of the next run,
  outside its measured time; the first run in a process starts its own.
- **Measurements.** Warm: 20 measured runs after 3 discarded, engines
  round-robin with a rotating start in one process. Cold: 8 fresh processes per
  cell. Memory: a separate pass, 5 fresh processes per cell. Percentiles are
  nearest-rank; times are ms at p50 / p95 of the whole task as the host saw it.
- **Correctness.** Every run's output must equal the expected value; failure
  scenarios must fail as specified. The ledger, kept by the tools themselves,
  is the ground truth for which operations started, completed, or took effect,
  and is reconciled against codemode's trace call by call.

Apple M1 Pro (10 cores, 32 GiB), macOS 14.4.1, Bun 1.4.4, Node 26.12.1, Deno
2.9.8. Raw runs and summaries: [`baseline`](../bench/results/baseline) at
`8cb7a63`, [`variants`](../bench/results/variants) (three builds interleaved),
[`after`](../bench/results/after) and [`holdout`](../bench/results/holdout) at
`3f7e5ab`.

## codemode before and after

Warm task time, p50 / p95 ms, all runs correct unless noted:

| Scenario | Latency | Before (`8cb7a63`) | After | After, prewarmed |
| --- | --- | ---: | ---: | ---: |
| triage | zero | 44.4 / 50.6 | 40.7 / 44.8 | **14.0 / 16.0** |
| triage | equal | 414.7 / 430.5 | 408.7 / 428.8 | **385.4 / 401.9** |
| triage | mixed | 821.9 / 828.7 | 816.9 / 823.4 | **784.6 / 796.0** |
| triage-large | zero | failed 0/20 | 47.4 / 51.4 | **20.0 / 21.1** |
| triage-large | mixed | failed 0/20 | 567.1 / 575.3 | **539.7 / 547.8** |
| denied | zero | 91.0 / 108.1 | 98.0 / 109.5 | **72.2 / 74.9** |
| denied | mixed | 863.3 / 878.6 | 859.8 / 877.4 | **830.6 / 841.8** |
| guest-exception | zero | 49.4 / 58.5 | 44.5 / 48.7 | **13.8 / 14.9** |
| deadline (250 ms) | mixed | 265.1 / 268.6 | 268.5 / 272.1 | 258.6 / 261.3 |
| lost-response (300 ms) | mixed | 314.4 / 318.4 | 318.8 / 321.2 | 308.4 / 312.3 |
| holdout `release-audit` | zero | — | 40.2 / 44.0 | **12.3 / 13.0** |
| holdout `release-audit` | mixed | — | 473.6 / 484.4 | **443.1 / 450.2** |

What each change bought:

| Change | Effect | Evidence |
| --- | --- | --- |
| Trace schema 2, host facts, inspection (`06ad723`, `de86444`) | No cost above run-to-run spread: −2.1 to +2.7 ms on 11 of 12 cells, +7.5 ms on triage/mixed at p50 with p95 1.7 ms lower | [`variants`](../bench/results/variants), interleaved |
| Frame copies instead of byte loops (`286b88d`) | −1.4 to −5.8 ms on triage, denied, and guest-exception; within ±1.3 ms elsewhere. 60 KB replies 1.0 → 0.30 ms per call in isolation | `variants`; commit message |
| Prewarmed guest (`1fc12d0`) | −23 to −32 ms on runs that complete (−26.7 ms on zero-latency triage); about −10 ms on deadline-bound runs. The guest's startup leaves the critical path | `after`, interleaved with unprewarmed codemode |
| 1 MiB replies (`abf1f29`) | `triage-large` from 0/20 to 20/20 | `baseline`, `after` |

Losses and costs:

- **Cold starts do not improve.** A fresh process still pays for the bridge and
  guest: 36.9 ms cold total for zero-latency triage, prewarmed or not.
- **Prewarming costs memory and a process pair.** An idle bridge and guest held
  about 37 MiB RSS (3 MiB bridge, 34 MiB Deno) while waiting. The deadline of a
  prewarmed run starts when the program is handed over.
- **Unprewarmed codemode got slower in three cells**: denied/zero +7.0 ms,
  lost-response +4.4 ms, and deadline +3.4 ms at p50. Separate runs of the same build differed by up to
  20 percent on zero-latency cells, so these are within the observed spread, but
  they are not improvements.
- **`triage-large` costs more than `triage`**: 375 KiB of results through the
  guest added about 6 ms at zero latency (20.0 vs 14.0 ms prewarmed).

## Compared with Pi and OpenCode

Warm task time, p50 ms, after the changes. Pi starts every call at once; codemode
and OpenCode run at most 8. The *model* column is the ideal time at concurrency 8
from the fixture latencies alone.

| Scenario | Latency | Model (8) | codemode | codemode, prewarmed | Pi | OpenCode |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| triage | zero | 0 | 40.7 | 14.0 | 23.0 | 13.5 |
| triage | equal | 330 | 408.7 | 385.4 | 284.9 | 380.0 |
| triage | mixed | 730 | 816.9 | 784.6 | 593.0 | 782.4 |
| triage-large | zero | 0 | 47.4 | 20.0 | 25.9 | 12.7 |
| triage-large | equal | — | 283.9 | 258.9 | 158.3 | 246.0 |
| denied | zero | — | 98.0 | 72.2 | 41.8 | 65.1 |
| guest-exception | zero | — | 44.5 | 13.8 | 23.3 | 8.6 |
| release-audit (holdout) | mixed | — | 473.6 | 443.1 | 300.6 | 437.7 |

- **At equal concurrency, prewarmed codemode is within 4 percent of OpenCode**
  on triage (+0.5, +5.4, +2.2 ms) and slower on `triage-large` (+7 to +13 ms) and
  `denied` (+7 ms). OpenCode runs in the host process with no subprocess boundary.
- **Pi is faster whenever latency matters** because it never queues a call: its
  ideal triage time is 240 ms at equal latency, against 330 ms for a cap of 8. It
  also applies no call allowance or concurrency limit.
- **Cold, codemode starts fastest**: zero-latency triage took 36.9 ms in a fresh
  process, against 49.2 ms for Pi and 194.5 ms for OpenCode, whose import of its
  dependencies dominates.
- **Memory sits in different places.** codemode added about 3 MiB to the host and
  ran in a bridge and guest peaking at 45 MiB together; Pi added 55 MiB and
  OpenCode 83 MiB to the host process itself (triage, mixed latency).

## What the ledger showed

- **Calls in flight at a deadline.** In `lost-response`, the label was applied in
  every run of every engine and no response arrived. codemode recorded the call
  `unknown`, with the host's `executor_started` fact, and `codemode inspect
  --reconcile` lists it for checking. Pi recorded it `cancelled`; OpenCode fired
  no end hook. In `deadline`, the page request in flight was `unknown` for
  codemode and `cancelled` for Pi.
- **No work after a run returned.** No engine started a tool after its run
  returned, in any of the timed runs.
- **Trace against ledger.** For codemode, every completed call matched a ledger
  completion, every undispatched call had no ledger start, and every unknown call
  matched a ledger start without a completion.

## Limits

- One machine, synthetic tools with fixed latencies, and five workflows. Real
  providers add variable latency, rate limits, and failures that these fixtures
  do not model.
- Pi and OpenCode run in the host process and are measured under Bun; Pi's own
  host is Node. A Node spot-check at `8cb7a63` moved Pi by +4.9 ms and codemode by
  +2.4 ms at zero latency, and both by under 1 percent under mixed latency.
- The programs use the JavaScript subset all three engines accept; codemode also
  runs TypeScript, which none of these programs needed.
- Cloudflare's executor ran locally in workerd under different conditions and is
  reported separately in [the comparison](comparison.md#cloudflare-measured-separately).
