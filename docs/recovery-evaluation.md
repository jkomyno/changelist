# Recovery evaluation

An offline, reproducible comparison of what happens after an executor is killed
in the middle of an approved plan, and of what durable accounting costs when
nothing fails. It answers two questions:

1. From one identical crashed state, which recovery strategy reaches the intended
   final state without duplicate effects, what does it leave unresolved, and how
   long does it take?
2. With no faults, what does journaling every attempt add to a dispatch run, per
   operation and per commit, under each durability setting?

Every dispatch uses the same finite native submission path, Rust scheduler, and
fixture. Approved operations need no JavaScript guest. This matches the runtime-only
and blind-retry baselines to native plan execution.

## Run it

```sh
mise exec -- cargo build --release --locked --bin codemode
mkdir -p eval/recovery-native
mise exec -- cargo run --release --locked --bin codemode-recovery-eval -- \
    --reps 15 --out eval/recovery-native > eval/recovery-native/summary.md
```

`codemode-recovery-eval` ([source](../src/bin/codemode-recovery-eval.rs)) needs the
release `codemode` binary next to it (or `--bin PATH`), because part A kills a real
`codemode plan run` process. It prints a Markdown summary and, with `--out`, writes
`summary.json`: every run's metrics, the aggregates, the configuration, and a build
fingerprint (checkout commit and dirty state, rustc, OS, CPU, binary SHA-256 digests,
and compiled evaluator, fixture, and recovery-plan source digests). Checkout metadata
is observed when the evaluation runs; binary and compiled source digests identify
the measured artifacts independently. The backend is recorded as `native`, and each
dispatch cell records its observed backend and total guest transport bytes.
The legacy `--deno PATH` option is accepted but unused; the harness does not discover
or launch Deno, including for version reporting. It exits 1 if a check that should hold
fails and 2 on a setup error. `tests/recovery_eval.rs` runs one repetition at 0 ms
latency in `mise run check`, using a nonexistent Deno path and checking the persisted
backend and absent startup evidence as well as fixture correctness.

## Method

### Matching parameters

| Parameter | Value, for every strategy and mode |
| --- | --- |
| Plan | [`examples/recovery/plan.json`](../examples/recovery/plan.json): 8 `add_label`, 2 `remove_label`, 1 `add_comment` (part B also uses a 64-operation `add_label` plan) |
| Concurrency | 4 operations in flight |
| Fixture latency | 0 ms and 25 ms per request, one profile at a time |
| Provider | the offline issue tracker in [`src/fixture.rs`](../src/fixture.rs), one SQLite file per run |
| Dispatch | `codemode::run_calls`, with the same finite native requests naming each operation as its tool plus `{"operation": id}`; rolling dispatch |
| Dispatch deadline | 60,000 ms, matching the approved session policy |
| Repetitions | 15 measured after 2 discarded warmups; strategies (A) and modes (B) run round-robin, and the starting one rotates every repetition |
| Statistics | nearest-rank percentiles; totals measured in µs and shown in ms; phases from the session's millisecond timings and the trace |

### A. How the crash is produced

For each repetition and latency profile, the harness creates a fresh fixture and
journal, approves the plan (concurrency 4, otherwise default policy, `storage`
durability), and arms four faults: lost responses after the effect commits for
`label-104`, `label-107`, and `comment-105`, and a held request (ledgered, never
applied) for `label-109`. It then starts the release `codemode plan run` as a
subprocess, polls the fixture ledger until all ten intended requests have arrived,
including the held request, and sends SIGKILL (`Child::kill`). By then all four
slots are stuck, so `label-110` was never
dispatched.

After the process is reaped, the harness copies each database and its write-ahead
log into three directories and checks the original: the journal must show
`label-101`, `-102`, `-103`, `-105`, `-106`, `-108` observed applied, the four faulted
operations unknown, and `label-110` pending; the ledger must hold 10 rows, one held.
Each strategy then recovers its own copy from that identical state:

- **Stop on uncertainty** reads the journal and assesses it. Any unknown operation
  stops it: it probes and sends nothing.
- **Blind retry** reads the journal once to learn which operations were not observed
  applied (the 4 unknown and the 1 pending), then sends all 5 again through the same
  native submission path with a plain provider that calls the fixture
  directly. It ignores recovery contracts, does no reconciliation, and writes no
  journal. Each request carries a fresh attempt key, so the fixture's receipts and
  fences do not deduplicate anything: this baseline has no idempotency keys. Reading
  the journal at all is generous; a baseline without durable accounting would know
  less.
- **Codemode resume** opens the journal with `storage` durability and calls
  `run_session` with `SessionKind::Resume` in process, against the copied fixture.

Metrics per strategy:

- *Final state correct*: `triage` on 101, 102, 104, 105, 106, 108, 109, 110;
  `needs-info` absent on 103 and 107; exactly one comment, on 105, with the planned
  body; no duplicate effect in the ledger.
- *Claims complete*: whether the strategy itself considers the plan finished.
- *Unresolved ops*, from the strategy's own knowledge. Stop: operations unknown or
  pending. Blind retry: operations it re-sent while an earlier attempt's outcome was
  unknown, since it cannot vouch that their effect happened once, plus any re-send
  not observed succeeding. Codemode: operations whose next step is not `done`.
- *Requests sent*: ledger rows added by the strategy.
- *Duplicate effects*: effects beyond the first for one logical change (same tool,
  issue, and label or body), over the whole ledger.
- *Repeats without effect*: requests beyond the first for one logical change that
  added no effect.
- *Recovery time*: wall time from the strategy's start (before opening the journal)
  to done, including closing the journal.
- *Guest startup*: absent for native execution, shown as `—`; no startup event or
  zero-duration guest phase is invented. *Dispatch execution*: the complete native
  run's elapsed time, including admission, scheduling, and provider work, in whole
  milliseconds. Stop dispatches nothing and has no execution timing.

Hard checks (exit 1): the crashed state above; codemode resume ends `needs_review`
with exactly `comment-105` open, a correct final state, and no duplicate; stop sends
no request. Blind retry's duplicate and stop's incomplete state are expected and
recorded, not failed.

### B. Persistence overhead

Each run gets a fresh fixture with no faults, and runs the whole plan once, in one
of three modes:

- **Runtime only**: `codemode::run_calls` with the same native requests and plain fixture
  provider as blind retry; no journal.
- **Journal, storage**: `run_session` with `Durability::Storage`
  (`synchronous=FULL`, and `F_FULLFSYNC` on macOS).
- **Journal, process**: `run_session` with `Durability::Process`
  (`synchronous=NORMAL`).

The journal is created and approved before the clock starts. *Total* is journal
open plus the session (or the run). *Commits* are counted from the journal after
the run: session begin and end, plus an intent and an observation per attempt.
The journal cost is paired by repetition: each mode's total minus the runtime-only
total of the same repetition, workload, and latency. The tables give the median of
those differences, divided by operations and by commits.

## Crash-checkpoint correction

The [initial native measurement](../eval/recovery-native-initial/summary.md)
failed its correctness oracle in one repetition: observing the held request
alone did not prove all earlier requests had reached the fixture. One was still
between its journal intent and provider entry when the process was killed.
That left a different crashed state and an unused injected fault, invalidating
the strategy comparison. Those results are retained as failed evidence and are
excluded from performance claims. The harness now waits for all ten intended
fixture requests, including the held one, before SIGKILL; the independent
post-kill state and ledger checks remain unchanged.

## Native results

The [corrected run](../eval/recovery-native/summary.md), with [all raw cells and
fingerprints](../eval/recovery-native/summary.json), passes every correctness
check across 270 measured cells. All 240 cells that dispatch work record backend
`native` and zero guest transport bytes. At both latency profiles, all 15 resume
runs reach the intended final state with no duplicate effects, leaving only the
uncertain comment for review.

| Metric | 0 ms provider latency | 25 ms provider latency |
| --- | ---: | ---: |
| Resume p50 / p90 | 70.9 / 75.3 ms | 94.1 / 100.9 ms |
| 64 operations, runtime only p50 | 31.3 ms | 447.9 ms |
| 64 operations, storage journal p50 | 642.4 ms | 671.1 ms |
| 64 operations, process journal p50 | 45.4 ms | 455.0 ms |

Measured on Apple M1 Pro, macOS, Rust 1.99.1 release, default features, using the
same 15 repetitions and two warmups described above. Guest startup is absent.
Storage durability still dominates the zero-latency journaled workload; the
paired journal overhead is 4.63 ms per commit at p50. Historical Deno figures
below used slower filesystem flushes on the same machine, so their total
before/after difference cannot all be credited to removing the guest. These
native baselines measure current persistence overhead against the same native
submission path.

## Historical Deno results

The tables and interpretation below describe the earlier Deno dispatch backend.
They remain historical evidence and do not measure the current native execution
path. In these tables, guest startup is measured from `guest_spawned` to the first
`admitted` event, and execution from first admission to the end of the run; the
native execution metric above instead covers the full dispatch run. Keep new native
results in a separate directory so these original artifacts remain unchanged.

Saved 2026-11-09: Apple M1 Pro (10 logical CPUs), macOS, Deno 2.9.8,
rustc 1.99.1, release build.
Measured code: commit `21c84f8`, after the review fixes that journal each probe's
start before the provider is asked. Full output:
[`eval/recovery/summary.md`](../eval/recovery/summary.md); every run:
[`eval/recovery/summary.json`](../eval/recovery/summary.json). An earlier run at
`ee34e8b`, before probes were charged at their start, measured codemode resume at
94 ms (0 ms latency) and 120 ms (25 ms latency) at p50.

### A. Correctness after the kill

Identical in all 15 runs of every cell, at both latencies:

| Strategy | Final state correct | Claims complete | Unresolved ops | Requests sent | Duplicate effects | Repeats without effect |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stop on uncertainty | 0/15 | 0/15 | 5 | 0 | 0 | 0 |
| blind retry | 0/15 | 15/15 | 4 | 5 | 1 | 3 |
| codemode resume | 15/15 | 0/15 | 1 | 2 | 0 | 1 |

- Stop leaves `label-109` and `label-110` unapplied and five operations it cannot
  account for.
- Blind retry posts the comment on 105 a second time in every run, while reporting
  success for every request. Its three harmless repeats are `label-104` and
  `label-107` (no change) and `label-109` (held, then applied).
- Codemode resume probed 3 operations (receipt for 104: applied; state for 107:
  satisfied; receipt for 109: not applied, and its old key fenced), dispatched 109
  and 110, and left `comment-105` for review, because the provider declares no
  reconciliation for comments and repeating one is unsafe. Its one repeat is the
  re-send of the held `label-109`.

### A. Recovery time (ms)

| Latency | Strategy | min | p50 | p90 | max |
| ---: | --- | ---: | ---: | ---: | ---: |
| 0 ms | stop on uncertainty | 2.9 | 3.2 | 3.6 | 3.6 |
| 0 ms | blind retry | 33.5 | 42.7 | 45.7 | 56.7 |
| 0 ms | codemode resume | 116.4 | 124.4 | 130.0 | 132.6 |
| 25 ms | stop on uncertainty | 3.2 | 3.5 | 4.1 | 5.3 |
| 25 ms | blind retry | 86.3 | 90.4 | 95.0 | 98.4 |
| 25 ms | codemode resume | 143.2 | 150.2 | 158.8 | 170.6 |

Phases, p50 in ms (startup columns first):

| Latency | Strategy | Journal open/read | Checks | Reconcile | Guest startup | Dispatch execution | Dispatch phase | Total |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 ms | stop on uncertainty | 3.2 | — | — | — | — | — | 3.2 |
| 0 ms | blind retry | 3.1 | — | — | 25 | 12 | 38 | 42.7 |
| 0 ms | codemode resume | 2.4 | 11 | 38 | 24 | 29 | 55 | 124.4 |
| 25 ms | stop on uncertainty | 3.5 | — | — | — | — | — | 3.5 |
| 25 ms | blind retry | 3.4 | — | — | 26 | 58 | 86 | 90.4 |
| 25 ms | codemode resume | 2.7 | 11 | 38 | 29 | 53 | 83 | 150.2 |

### B. No-fault runs (ms)

| Workload | Latency | Mode | Total p50 / p90 | Dispatch p50 / p90 | Journal open p50 | Checks p50 | Guest startup p50 | Execution p50 / p90 | Commits |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 11 ops | 0 ms | runtime only | 43.8 / 56.2 | 43 / 55 | — | — | 24 | 13 / 23 | — |
| 11 ops | 0 ms | journal, storage | 189.6 / 217.1 | 163 / 185 | 0.5 | 12 | 28 | 133 / 139 | 24 |
| 11 ops | 0 ms | journal, process | 53.9 / 60.4 | 50 / 57 | 0.5 | 1 | 33 | 13 / 26 | 24 |
| 11 ops | 25 ms | runtime only | 118.4 / 119.7 | 117 / 118 | — | — | 27 | 88 / 90 | — |
| 11 ops | 25 ms | journal, storage | 221.4 / 232.5 | 193 / 202 | 0.6 | 12 | 31 | 157 / 161 | 24 |
| 11 ops | 25 ms | journal, process | 123.9 / 129.3 | 120 / 126 | 0.5 | 1 | 31 | 89 / 90 | 24 |
| 64 ops | 0 ms | runtime only | 55.6 / 76.2 | 55 / 75 | — | — | 24 | 29 / 46 | — |
| 64 ops | 0 ms | journal, storage | 927.8 / 955.5 | 891 / 923 | 0.7 | 12 | 28 | 860 / 888 | 130 |
| 64 ops | 0 ms | journal, process | 72.8 / 86.6 | 68 / 82 | 0.8 | 1 | 34 | 30 / 44 | 130 |
| 64 ops | 25 ms | runtime only | 523.1 / 548.1 | 521 / 546 | — | — | 35 | 484 / 509 | — |
| 64 ops | 25 ms | journal, storage | 934.4 / 984.8 | 897 / 946 | 1.3 | 12 | 42 | 849 / 896 | 130 |
| 64 ops | 25 ms | journal, process | 545.2 / 565.4 | 541 / 560 | 0.8 | 1 | 34 | 506 / 524 | 130 |

Journal cost against the runtime alone, paired by repetition:

| Workload | Latency | Durability | Δ total p50 / p90 (ms) | Per op (ms) | Per commit (ms) | Δ execution p50 / p90 (ms) |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 11 ops | 0 ms | storage | 149.9 / 170.2 | 13.63 | 6.25 | 119 / 129 |
| 11 ops | 0 ms | process | 8.5 / 28.6 | 0.77 | 0.35 | 0 / 18 |
| 11 ops | 25 ms | storage | 106.6 / 116.9 | 9.69 | 4.44 | 68 / 74 |
| 11 ops | 25 ms | process | 5.5 / 17.1 | 0.50 | 0.23 | 2 / 3 |
| 64 ops | 0 ms | storage | 870.0 / 895.6 | 13.59 | 6.69 | 825 / 844 |
| 64 ops | 0 ms | process | 18.4 / 36.5 | 0.29 | 0.14 | 1 / 18 |
| 64 ops | 25 ms | storage | 401.2 / 469.1 | 6.27 | 3.09 | 361 / 407 |
| 64 ops | 25 ms | process | 27.4 / 62.7 | 0.43 | 0.21 | 20 / 46 |

All 90 recovery runs and 180 no-fault runs passed their checks.

## What the measurements show

- **Only codemode resume reached the intended state without a duplicate.** It did
  so by sending 2 requests where blind retry sent 5, and by stopping short of the
  one operation it could not establish. That leaves work for a person: the comment
  did apply, but nothing the provider offers can show it, so codemode reports
  `needs_review` rather than `completed`.
- **Blind retry is faster and wrong in a way it cannot see.** It finished 82 ms
  sooner at 0 ms latency and 60 ms sooner at 25 ms, claimed success in every run, and
  duplicated the comment in every run. Label operations survived repetition only
  because the fixture's labels are a set.
- **Stop is cheap and incomplete.** It took about 3 ms and changed nothing, which is
  safe for the comment and leaves two operations that could safely have been sent.
- **Codemode resume is the slowest strategy here (a regression against blind
  retry).** At 0 ms latency, its 124 ms split into journal open (2 ms), checks
  (11 ms: a durable session-start commit and authorization), reconciliation
  (38 ms: three probes, each recorded twice with storage-durable commits, at its
  start and with its answer), and dispatch (55 ms, of which 24 ms was guest
  startup). The rest is assessment and the durable session end. At about 6 ms per
  storage commit (part B), the session-start commit, six probe records, two
  intents, two observations, and the session end account for most of the gap.
  Charging each probe before the provider is asked, so a crash loop cannot probe
  without bound, added about 30 ms against the earlier measurement.
- **Guest startup dominates small batches.** Starting the Deno guest took 24–42 ms
  in every dispatch run, more than half of blind retry's recovery at 0 ms and of
  the 11-operation runtime-only run.
- **Storage durability costs about 6 ms per commit on this machine.** Without
  latency, the 11-operation plan took 190 ms instead of 44 ms (24 commits), and the
  64-operation plan 928 ms instead of 56 ms (130 commits). Commits go through one
  connection in order, so with `storage` the journal, not the provider, limits
  throughput: the 64-operation run took about 930 ms at both 0 and 25 ms latency.
  With latency, some commit time overlaps calls that are waiting, so the measured
  cost per commit falls to 3.1–4.4 ms.
- **Process durability costs little here.** The paired median was 5.5–27.4 ms per
  run (at most 0.77 ms per operation), within the run-to-run spread: paired p90
  reached 63 ms.
- **Startup is separate from execution.** Opening and locking the journal took
  about 1 ms. The opening checks took 12 ms with `storage` and about 1 ms with
  `process`, mostly the durable session-start commit.

## Limits

- One machine (Apple M1 Pro, macOS), one Deno version, 15 repetitions.
  Timestamps in the phase columns are whole milliseconds.
- An offline fixture with synthetic, fixed latency. Real providers add variable
  latency, rate limits, and failures this harness does not model.
- One crash shape: a process killed with SIGKILL while four requests pend. Other
  interleavings (a kill between intent and request, during reconciliation, or during
  the journal's own commit) are not measured here.
- Process crashes only. `storage` asks the OS to flush (on macOS with
  `F_FULLFSYNC`); whether that survives power loss depends on the device and is not
  tested. These numbers measure the cost of the settings, not power-loss behavior.
  The fixture uses `synchronous=FULL` without `fullfsync` for its own commits in
  every mode.
- `applied` and `not_applied` are decidable only because the fixture keeps
  receipts per attempt key and fences keys it reports as not applied. A provider
  without receipts or fences leaves those operations for review, as the comment
  was here.
- Blind retry is one baseline, without idempotency keys. A retry that reused stable
  idempotency keys with a provider that honors them would avoid the duplicate
  comment; it would still need a way to know which operations to send.
- The 64-operation plan has only label additions, which are repeat-safe and have
  receipts; it measures journaling cost, not recovery.
